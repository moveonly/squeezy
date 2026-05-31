# Dart language-implementation spec

Target branch: `langs/dart`. Scaffold (`LanguageKind::Dart`, `LanguageFamily::Dart`, extension `.dart`, placeholder `extract_dart` at `crates/squeezy-parse/src/languages/dart.rs`) already landed; this spec defines the follow-up implementation commit.

## 1. Template choice

**Primary template: JS/TS** (`crates/squeezy-parse/src/languages/js_ts.rs`, `crates/squeezy-graph/src/languages/js_ts.rs`). Dart's surface syntax — `class Foo extends Bar implements Baz`, `void method() { ... }`, `async`/`await`, `import 'package:flutter/material.dart' as m show X hide Y`, and dotted module paths — is structurally closer to TypeScript than to anything else squeezy currently supports. Mixed static/dynamic typing (`dynamic`, type inference, optional null types) parallels TypeScript's `any` / inferred types, so JS/TS's heuristic call-receiver resolution and reference-vs-declaration disambiguation transfer directly. Import grammar (`show`/`hide`, default vs named vs namespace) maps onto JS's `import { a, b as c } from 'mod'` shape with minor renaming.

**Secondary template: Java** (`crates/squeezy-parse/src/languages/java.rs`). Class semantics — single inheritance plus interfaces, `implements`, constructor signatures distinct from method signatures, the `base:<Name>` attribute pattern for ancestor lookup — line up with Java more cleanly than with JS/TS. Mixins (`with Bar, Baz`) act like additional interfaces for ancestor resolution and reuse Java's `base:` attribute machinery (renamed `mixin:` to distinguish, see §4b). Steal Java's `signature_text` shape (declaration line up to but excluding the body brace), Java's `extract_java_method_invocation` arity-from-named-children pattern, and Java's `dedup_java_facts` byte-span uniqueness path.

The Dart extractor is therefore a JS/TS-shaped visitor (`visit_dart_node` modelled on `visit_js_ts_node`) whose symbol-from-node table borrows Java's class-family branches, with Dart-specific synthesis (parts, mixins, extensions) layered on top.

## 2. Grammar

**Recommendation: `tree-sitter-dart` from `UserNobody14/tree-sitter-dart`** — the most-active maintained Dart grammar (the official Dart team does not publish one). **Flag for verification**: confirm latest tagged release at implementation time; as of writing this spec the project ships pre-1.0 (`0.0.x`) tags. Pin via:

```toml
tree-sitter-dart = { git = "https://github.com/UserNobody14/tree-sitter-dart", rev = "<verified-sha>" }
```

A git rev pin (not a crates.io version) is required because the maintainer publishes tagged commits but has historically been sporadic on crates.io. Verify the chosen rev parses the §6 fixture cleanly with zero `ERROR` nodes before merging. The Rust-side language constructor lives next to its siblings in `crates/squeezy-parse/src/lib.rs`:

```rust
fn dart_language() -> tree_sitter::Language {
    tree_sitter_dart::language()
}
```

Notable grammar quirks the visitor must tolerate:

- `library_name`, `import_specification`, `export_specification`, `part_directive`, `part_of_directive` are siblings of class declarations at the top level — descend the program root linearly.
- Field declarations use `initialized_identifier_list` inside a `declaration` parent; iterate identifiers similarly to Java's `variable_declarator` loop.
- Function bodies come in three shapes: `function_body` (block), `=>` expression body, and abstract / external (no body). The `body_span` is `Some` only for the first two.
- `formal_parameter_list` includes `normal_formal_parameter`, `optional_formal_parameters`, and `named_formal_parameters`; arity is the named-child count of the whole list (matches Java's path).
- Type arguments (`type_arguments`) decorate identifier nodes — strip them when extracting receiver/target text.
- Cascaded calls (`obj..foo()..bar()`) parse as a `cascade_section` chain; descend so each call is captured.
- String interpolation (`'hi $name ${x.y()}'`) produces `interpolation` nodes whose children are real expressions — descend so calls/references inside interpolations are captured (mirrors Ruby).
- `ERROR`/missing nodes emit a `Partial` `ParseDiagnostic`, identical to the JS/TS diagnostic path.

## 3. AST-node to fact mapping

| tree-sitter node kind | Fact emitted | SymbolKind | Notes |
|---|---|---|---|
| `library_name` | `ParsedImport` (`alias = Some("__dart_library__")`, `is_reexport = true`) | — | path is the dotted library name; consumed by the package detector mirroring Java's `__java_package__` trick. |
| `import_specification` | `ParsedImport` | — | path = unwrapped URI string; alias from `as` clause; `show`/`hide` lists become extra Named imports per name (§4l). Conditional `if (dart.library.X) 'alt.dart'` -> primary Named, alt Named with `Confidence::Partial` (§4m). |
| `export_specification` | `ParsedImport` (`is_reexport = true`) | — | same path/show/hide handling as import. |
| `part_directive` (`part 'other.dart';`) | `ParsedImport` (`kind = ImportKind::Wildcard`, `alias = Some("__dart_part__")`, `is_reexport = true`) | — | path = relative URI string; the graph resolver attaches the part's symbols to this file's library (§4a). |
| `part_of_directive` (`part of 'main.dart';` or `part of name;`) | `ParsedImport` (`alias = Some("__dart_part_of__")`, `is_static = true`, `is_reexport = true`) | — | resolver follows back to the host library so the host's classes adopt the part's members. |
| `class_definition` | `ParsedSymbol` | `Class` | name from `name` field; base class via `superclass` field -> `ReferenceKind::Type` + attribute `base:<Name>`; interfaces from `implements` clause -> attribute `iface:<Name>`; mixins from `with` clause -> attribute `mixin:<Name>` per name (§4b). |
| `mixin_declaration` | `ParsedSymbol` | `Trait` | mirrors Java's `interface_declaration`; `on` clause (`mixin M on Foo`) emits attribute `mixin-on:Foo`. |
| `extension_declaration` (`extension MyExt on int`) | `ParsedSymbol` | `Class` | name from `name` field (anonymous extensions get a synthetic name `__ext_<line>_<col>` and attribute `dart:anonymous-extension`); `on` type goes into `language_identity` so methods inside resolve against the receiver type (§4d). Attribute `dart:extension`. |
| `extension_type_declaration` (Dart 3.0+, `extension type Wrapper(int value)`) | `ParsedSymbol` | `Class` | `Confidence::High`; attribute `dart:extension-type`; representation field synthesized as a `Field` child (§4c). |
| `enum_declaration` | `ParsedSymbol` | `Enum` | constant entries become child `Const` symbols. Enhanced enums (Dart 2.17+) may declare methods/constructors — emit as Method children of the enum. |
| `function_signature` / top-level `function_signature` | `ParsedSymbol` | `Function` if at library top-level, otherwise `Method` | arity from `formal_parameter_list`. |
| `method_signature` | `ParsedSymbol` | `Method` | inside a class/mixin/extension body. |
| `getter_signature` | `ParsedSymbol` | `Variable` | attribute `dart:getter`; arity 0; `signature_text` includes `get` keyword (§4f). |
| `setter_signature` | `ParsedSymbol` | `Method` | attribute `dart:setter`; arity 1; name preserves identifier *without* `=` suffix (Dart setter names have no `=`, unlike Ruby). |
| `constructor_signature` | `ParsedSymbol` | `Method` | attribute `dart:constructor`. Named constructors keep the full `Foo.named` form in `name` (§4g). |
| `factory_constructor_signature` | `ParsedSymbol` | `Method` | attributes `dart:constructor`, `dart:factory` (§4h). |
| `type_alias` (`typedef X = ...`) | `ParsedSymbol` | `Trait` | matches Java's annotation-type-declaration mapping; attribute `dart:typedef`. |
| `field_declaration` | one `ParsedSymbol` per identifier | `Field` | mirrors `java_field_symbols_from_node`; attribute `type:<TypeName>` when annotation present, `dart:static` if `static`. |
| `top_level_variable_declaration` | `ParsedSymbol` | `Const` if `const`/`final` with literal rhs, otherwise `Variable` | mirrors Ruby `Const` heuristic. |
| `function_invocation` | `ParsedCall` + `BodyHit::Call` | — | `Confidence::Heuristic`; arity from `arguments`. |
| `method_invocation` (`obj.foo(...)`, `obj?.foo(...)`, `obj..foo(...)`) | `ParsedCall` + `BodyHit::Call` | — | `kind = ParsedCallKind::Method`; receiver from the receiver field; `Confidence::CandidateSet`. |
| `new_expression` / `constructor_invocation` (`Foo()`, `Foo.named()`, `new Foo()`) | `ParsedCall` (`Direct`) + `BodyHit::Call` | — | mirrors Java's `extract_java_object_creation`. |
| `type_name` / `qualified` (in type position) | `ParsedReference` + `BodyHit::Type` | — | `ReferenceKind::Type`. |
| `identifier` (bare reference, not a declaration name) | `ParsedReference` + `BodyHit::Identifier` | — | `ReferenceKind::Identifier`. |
| `string_literal`, `numeric_literal`, `boolean_literal`, `null_literal`, `symbol_literal` | `BodyHit::Literal` | — | mirrors `is_js_ts_literal`. |
| any `ERROR`/missing node | `ParseDiagnostic` (`Partial`) | — | identical to JS/TS diagnostic path. |

## 4. Language gotchas & heuristics

**(a) Library parts (`part` / `part of`).** Dart libraries can be split across files using `part 'other.dart';` in the host and `part of 'main.dart';` (or `part of host.library.name;`) in each part. Parts share the host library's scope: a private name `_x` in the host is visible in every part, and members declared in the part attach to the host's library symbol table. The extractor emits both directives as `ParsedImport` markers (see §3) with sentinel aliases `__dart_part__` and `__dart_part_of__`. The graph resolver (`crates/squeezy-graph/src/languages/dart.rs`) follows `part of` back to the host file and rewrites the part's top-level symbols' `parent_id` to point at the host's library symbol. **Critical for Flutter**: state-management codegen frequently uses this pattern (e.g. `freezed`/`riverpod_generator` generated `*.g.dart` files that declare themselves `part of` the main file).

**(b) Mixins (`class Foo with Bar, Baz`).** The `with` clause introduces every named mixin as a `ReferenceKind::Type` on the host class plus a `mixin:<Name>` attribute, mirroring Ruby's `mixin:include:` attribute pattern. The Dart graph resolver mirrors `java_type_inheritance_names`-style ancestor lookup but extends it: ancestor walk goes `mixin:` (Dart applies mixins right-to-left, so iterate the `with` list in reverse before the superclass) -> `base:` -> `iface:`, capped at depth 8. A `mixin M on Foo { ... }` declaration emits `mixin-on:Foo` as an additional ancestor for method resolution.

**(c) Extension types (Dart 3.0+).** `extension type Wrapper(int value) { ... }` declares a zero-overhead wrapper around `int`. Emit as a `Class` with `Confidence::High`, attribute `dart:extension-type`, and synthesize the representation parameter (`int value`) as a child `Field` symbol. Members inside resolve against the wrapper type, not against `int`.

**(d) Extensions (`extension MyExt on int { ... }`).** Emit the extension itself as a `Class` symbol with attribute `dart:extension`. Critically: set `language_identity = Some("int")` (or whatever the `on` type is) so the resolver can match calls of the form `someInt.myMethod()` to extension methods declared `on int`. Methods inside the extension get `SymbolKind::Method` with the extension as `parent_id` *and* inherit the parent's `language_identity` — the graph dispatches via "extension on T" lookup before falling back to "method on T's class".

**(e) Sealed classes (Dart 3).** `sealed class Result<T> { ... }` plus child `class Ok<T> extends Result<T>` / `class Err extends Result<Never>` emit normally — the children's `base:Result` attribute is sufficient for the graph to walk up. Add attribute `dart:sealed` on the parent so query consumers can identify exhaustive switches; no extra references are synthesized beyond what `extends` already produces.

**(f) Getters and setters.** Getters become `SymbolKind::Variable` with attribute `dart:getter` (clients reading `foo.bar` see a Variable-shaped lookup). Setters become a separate `SymbolKind::Method` with attribute `dart:setter` and arity 1. Both share the same `name` (Dart, unlike Ruby, does not append `=` to setter names). The graph resolver treats a write context (`obj.bar = x`) as a setter dispatch and a read context as a getter dispatch — for the first PR, both kinds are emitted with `Confidence::High` declarations and the dispatcher picks based on `BodyHit` context at query time.

**(g) Named constructors (`Foo.named()`).** Dart allows multiple constructors per class distinguished by name: `Foo();`, `Foo.fromJson(Map m);`, `Foo.empty();`. Each emits a `SymbolKind::Method` whose `name` is the full dotted form (`Foo.fromJson`) — this matches how callers write the invocation. Attribute `dart:constructor` so signature search can filter constructors out of method results.

**(h) Factory constructors (`factory Foo()`).** Emit as `SymbolKind::Method` with both `dart:constructor` and `dart:factory` attributes. The body may return any subtype, so resolver call-site matching uses the declared name only (not return-type analysis).

**(i) Async kinds (`async`, `async*`, `sync*`).** Emit the function/method symbol unchanged; record the modifier as an attribute (`dart:async`, `dart:async-star`, `dart:sync-star`) so call-chain queries can filter generators. The visitor descends through `await`/`yield` expressions transparently; the call inside `await someFuture()` is captured as a normal `ParsedCall`.

**(j) `noSuchMethod` override.** Declarations of `noSuchMethod` emit as a normal Method. Runtime dispatch *via* `noSuchMethod` (i.e. calls to methods that don't statically exist but are routed at runtime) is **excluded entirely** — both sides of the oracle comparison should suppress it. Documented as a known recall gap (mirrors Ruby's `method_missing` stance).

**(k) Codegen (`*.g.dart`, `*.freezed.dart`).** Build-time generated files (json_serializable, freezed, riverpod_generator, mockito, build_runner) contain synthesized symbols that are typically uninteresting for navigation and dominate FN counts on real Flutter corpora. **First PR**: exclude them from the FN/FP accounting via the `default_oracle_exclusions` glob (`**/*.g.dart`, `**/*.freezed.dart`, `**/*.mocks.dart`) — the squeezy backend still parses and emits them so they appear in hierarchy queries, but precision/recall numbers are computed over a filtered corpus. Compare separately in a follow-up PR. This matches the Ruby `vendor/` + `generated/` fallback-exclusion approach.

**(l) Imports with prefixes, `show`, `hide`.** `import 'package:foo/bar.dart' as f show baz hide qux;` is decomposed into:
- one `ParsedImport { path: "package:foo/bar.dart", alias: Some("f"), kind: Namespace }` (the prefix binding),
- one Named import per name in `show`: `ParsedImport { path: "package:foo/bar.dart.baz", alias: None, kind: Named, imported_name: Some("baz") }`,
- `hide` names are **excluded** (no import recorded for them).

The order matches the JS/TS extractor's named-import decomposition shape.

**(m) Conditional imports.** `import 'x.dart' if (dart.library.io) 'y_io.dart' if (dart.library.html) 'y_html.dart';` emits one primary `ParsedImport` for `x.dart` (`Confidence::High`) plus one for each alternate (`Confidence::Partial`, attribute `dart:conditional-alternate`). Resolver prefers the primary when both targets exist in the workspace; if only an alternate exists, it falls back to it with the Partial confidence preserved.

## 5. Per-symbol confidence rules

| Situation | Confidence |
|---|---|
| `class_definition` / `mixin_declaration` / `enum_declaration` / `extension_type_declaration` | `High` |
| `extension_declaration` with named identifier | `High` |
| Anonymous `extension on T` (no name) | `Partial` (synthesized name) |
| `method_signature`, `function_signature`, `constructor_signature`, `factory_constructor_signature` with body | `High` |
| `abstract` / `external` declarations (no body) | `High` (declaration is still authoritative) |
| `getter_signature` / `setter_signature` | `High` |
| `field_declaration` per identifier | `High` |
| `top_level_variable_declaration` with literal/constant rhs | `High` |
| `top_level_variable_declaration` with call expression rhs | `Partial` |
| `noSuchMethod` override declaration | `High`; runtime dispatch via it is **excluded** (no Partial emission) |
| Codegen file (`*.g.dart`, `*.freezed.dart`, `*.mocks.dart`) symbol | `High` (still emitted); excluded from FP/FN via glob (§4k) |
| Conditional import alternate target | `Partial` |
| File contains parse `ERROR` node | per-symbol confidence unchanged; file gets `ParseDiagnostic` with `Partial` (mirrors JS/TS) |
| `function_invocation` | `Heuristic` |
| `method_invocation` (any receiver) | `CandidateSet` |
| `new_expression` / `constructor_invocation` | `Heuristic` |

## 6. Fixture sketch

Layout under `benchmarks/fixtures/dart/semantic-cases/`:

- `lib/src/network/client.dart`
  - `library network.client;`
  - `part 'response.dart';`
  - `class HttpClient { Future<Response> fetch(String url) async { ... } }`
  - exercises library + part directive, async function, cross-file call into `Response`.
- `lib/src/network/response.dart`
  - `part of 'client.dart';`
  - `class Response { final int status; final String body; Response(this.status, this.body); }`
  - exercises part-of resolution — the resolver must attach `Response` to the `network.client` library so queries for "symbols in client.dart's library" find it.
- `lib/src/auth/state.dart`
  - `sealed class AuthState { const AuthState(); }`
  - `class SignedIn extends AuthState { final String userId; const SignedIn(this.userId); }`
  - `class SignedOut extends AuthState { const SignedOut(); }`
  - exercises sealed-class + subclass references + named/positional constructors.
- `lib/src/util/loggable.dart`
  - `mixin Loggable { void log(String msg) { print('[$runtimeType] $msg'); } }`
  - referenced by `class Service with Loggable` in `lib/src/services/service.dart` — exercises cross-file mixin resolution.
- `lib/src/services/service.dart`
  - `import 'package:fixture/src/util/loggable.dart' show Loggable;`
  - `import 'package:fixture/src/network/client.dart' as net;`
  - `class Service with Loggable { Future<void> run() async { final c = net.HttpClient(); final r = await c.fetch('/x'); log('got ${r.status}'); } }`
  - exercises mixin method call (`log`), prefixed namespace call (`net.HttpClient`), async call chain (`fetch -> async`).
- `lib/src/util/string_ext.dart`
  - `extension StringExt on String { String shout() => toUpperCase() + '!'; }`
  - referenced by `'hello'.shout()` in `lib/main.dart` — exercises extension-method resolution against `String`.
- `lib/main.dart`
  - `import 'package:fixture/src/services/service.dart';`
  - `import 'package:fixture/src/util/string_ext.dart';`
  - `void main() async { final s = Service(); await s.run(); print('hi'.shout()); }`
  - top-level Function, cross-file chains, extension call.
- `vendor/ignored.dart`
  - `void vendoredShadow() => print('vendor');` — exercises vendor-dir exclusion (mirrors Ruby fixture).
- `generated/widget.g.dart`
  - `// GENERATED CODE - DO NOT MODIFY BY HAND` header + a stub freezed-style class — exercises codegen fallback exclusion (§4k).

This gives one mixin + one extension + four classes + a sealed hierarchy + cross-file part-of + cross-file mixin/extension chains across three top-level dirs plus vendor/generated decoys; enough to exercise hierarchy, signature search, references, call chain, fallback quality, part-of resolution, and import-prefix resolution queries.

## 7. Real-repo corpus

- **Primary repo**: `https://github.com/flutter/packages` (the `flutter/plugins` repo was merged into `flutter/packages` upstream; verify the canonical URL at implementation time).
- **Suggested rev**: latest commit on `main` at corpus-add time; pin to a specific SHA in `benchmarks/corpus.json`.
- **Smoke subset**: `packages/path_provider/path_provider/lib/` and `packages/path_provider/path_provider/lib/src/` — roughly 50-100 Dart files of pure-Dart facade code that fans out to platform-channel implementations. Use a `subdir` field on the corpus repo entry (or check out the full repo and rely on workspace crawl exclusions if `subdir` is not yet supported by the corpus loader — confirm against the loader before writing the entry; same caveat the Ruby spec notes).
- **Why path_provider**: idiomatic Dart/Flutter package, uses platform channels (which produce interesting cross-file `MethodChannel.invokeMethod` calls and platform-interface implementations), reasonable size, mostly pure Dart (Java/Swift/Kotlin side files are filtered by `.dart` extension), well-documented public API. The platform-channel pattern stresses the extractor's handling of `async` chains and inter-class delegation.
- **Alternative**: `https://github.com/dart-lang/path` (small, pure Dart, no Flutter dependencies, idiomatic stdlib-style code) if `path_provider` proves too Flutter-heavy or introduces too much noise from generated files. Use this as a fallback for the smoke corpus if Flutter SDK setup in CI is prohibitive.

## 8. Smoke query spec

File: `benchmarks/specs/dart-smoke-queries.json`. Contents:

```json
{
  "queries": [
    {
      "id": "dart-hierarchy",
      "kind": "hierarchy_contains",
      "expected_contains": [
        "Class:HttpClient",
        "Class:Response",
        "Class:AuthState",
        "Class:SignedIn",
        "Class:SignedOut",
        "Class:Service",
        "Class:StringExt",
        "Trait:Loggable",
        "Method:fetch",
        "Method:run",
        "Method:log",
        "Method:shout",
        "Function:main"
      ]
    },
    {
      "id": "dart-class-signature",
      "kind": "signature_search",
      "text": "class HttpClient",
      "symbol_kind": "Class",
      "expected_contains": [
        "Class:HttpClient"
      ]
    },
    {
      "id": "dart-named-constructor-signature",
      "kind": "signature_search",
      "text": "const SignedIn(this.userId)",
      "symbol_kind": "Method",
      "attribute": "dart:constructor",
      "expected_contains": [
        "Method:SignedIn"
      ]
    },
    {
      "id": "dart-mixin-references",
      "kind": "references_to_symbol",
      "to": "Loggable",
      "expected_contains": [
        "Loggable"
      ]
    },
    {
      "id": "dart-async-call-chain-cross-file",
      "kind": "call_chain",
      "from": "run",
      "to": "fetch",
      "expected_contains": [
        "run -> fetch"
      ]
    },
    {
      "id": "dart-mixin-method-call-chain",
      "kind": "call_chain",
      "from": "run",
      "to": "log",
      "expected_contains": [
        "run -> log"
      ]
    },
    {
      "id": "dart-extension-method-resolution",
      "kind": "call_chain",
      "from": "main",
      "to": "shout",
      "expected_contains": [
        "main -> shout"
      ]
    },
    {
      "id": "dart-part-of-resolution",
      "kind": "hierarchy_contains",
      "expected_contains": [
        "Class:Response in library network.client"
      ]
    },
    {
      "id": "dart-import-prefix",
      "kind": "signature_search",
      "text": "import 'package:fixture/src/network/client.dart' as net",
      "expected_contains": [
        "import"
      ]
    },
    {
      "id": "dart-fallback-quality",
      "kind": "fallback_quality",
      "expected_contains": [
        "generated",
        "vendor"
      ]
    }
  ]
}
```

The `dart-part-of-resolution` query specifically verifies that `Response` (declared in `response.dart`) is reported as belonging to the `network.client` library — this is the resolver's part-of attachment in action and is the highest-value Dart-specific query.

## 9. Oracle plan

- **Tool**: **`dart analyze --format=machine`** for diagnostics, **`package:analyzer`** (the Dart SDK's canonical static analysis library) for the symbol table. Justification: `package:analyzer` is the analysis engine that powers `dart analyze`, the Dart Language Server, IntelliJ's Dart plugin, and `dart fix`; it is the canonical source of Dart's element model (`LibraryElement`, `ClassElement`, `MethodElement`, etc.). Alternatives considered and rejected: `dart_style` (formatting-focused, no semantic model), hand-rolled regex (intractable given Dart's syntax), `kythe` extractors (heavy, build-system-coupled).
- **Helper layout**: a small Dart program at `benchmarks/oracle-helpers/dart-oracle/` with `pubspec.yaml`:

  ```yaml
  name: dart_oracle
  description: Squeezy benchmark oracle for Dart corpora.
  environment:
    sdk: ^3.0.0
  dependencies:
    analyzer: ^7.0.0
    path: ^1.9.0
  ```

  and `bin/dart_oracle.dart` whose entry point walks the supplied source root, builds an `AnalysisContextCollection` over it, iterates each library's `units` and the `CompilationUnitElement` for each unit, and emits JSON to stdout:

  ```json
  {
    "rows": [
      ["lib/src/network/client.dart", "Library", "network.client"],
      ["lib/src/network/client.dart", "Class", "HttpClient"],
      ["lib/src/network/client.dart", "Method", "fetch"],
      ["lib/src/network/response.dart", "Class", "Response"],
      ["lib/src/auth/state.dart", "Class", "AuthState"],
      ["lib/src/util/loggable.dart", "Mixin", "Loggable"],
      ["lib/src/util/string_ext.dart", "Extension", "StringExt"]
    ],
    "unparseable_files": []
  }
  ```

  Element walk visits: `LibraryElement.identifier` (one Library row per library, **once** — `part`-file units do not get their own Library row; their members are emitted under the host library's URI), `ClassElement` (Class), `MixinElement` (Mixin -> mapped to `Trait` on the squeezy side), `ExtensionElement` (Extension -> Class with `dart:extension`), `ExtensionTypeElement` (ExtensionType -> Class with `dart:extension-type`), `EnumElement` (Enum), `TopLevelFunctionElement` (Function), `MethodElement` (Method), `ConstructorElement` (Method with `dart:constructor`), `PropertyAccessorElement.isGetter` (Variable with `dart:getter`), `PropertyAccessorElement.isSetter` (Method with `dart:setter`), `FieldElement` (Field), `TopLevelVariableElement` (Const or Variable). One Dart process per scan — never per-file.

- **Install in CI**: `dart-lang/setup-dart@v1` GitHub Action pinned to Dart 3.x (`sdk: stable`). Wrap in `continue-on-error: true` so a Dart SDK miss degrades gracefully (see "Scan-only fallback" below).
- **Scan command**: in the workflow, immediately after `setup-dart`, run `dart pub get` once in `benchmarks/oracle-helpers/dart-oracle/`. Then the Rust oracle wrapper invokes `dart run benchmarks/oracle-helpers/dart-oracle/bin/dart_oracle.dart <source-root>` and parses the JSON. The Rust side adds `benchmarks/squeezy-graph-bench/src/oracles/dart_analyzer.rs` modelled on `oracles/cpython_ast.rs`, and `collect_dart_analyzer_symbol_scan` in `common_scan.rs`.
- **Exclusion list** (the oracle does **not** emit these; the squeezy side suppresses them symmetrically via `default_oracle_exclusions`):
  - locals, block-local variables, closures (anonymous function expressions)
  - generated files: `**/*.g.dart`, `**/*.freezed.dart`, `**/*.mocks.dart` (compared separately in a follow-up PR; §4k)
  - part-file synthesized members: the analyzer surfaces part members **only** under the host library element. The squeezy resolver re-parents part members to the host's library symbol (§4a), so the comparison key must use the **host file** for part-member rows. Without this, squeezy will double-count or mismatch on the part-vs-host file column. Implement as an oracle-side rewrite: when the analyzer reports a symbol declared in `response.dart` but whose enclosing `LibraryElement.firstFragment.source` is `client.dart`, emit `["lib/src/network/client.dart", "Class", "Response"]` rather than `["lib/src/network/response.dart", ...]`. Squeezy applies the same rewrite via the `__dart_part_of__` import marker.
  - `noSuchMethod`-routed runtime methods (declarations of `noSuchMethod` itself are emitted; dispatched-via-`noSuchMethod` calls are excluded from both sides)
- **Definition/reference probes**: declarations *and* references via the analyzer's element model (`element.session.getResolvedUnit(unit.source.fullName)` plus an `AstVisitor` over the resolved AST that records `SimpleIdentifier.staticElement` for each reference). Set `ra_lsp_probes: 25` to match Rust's smoke probe budget — the analyzer's resolution is fast enough on the smoke corpus to support reference probes from the first PR (unlike Ruby Prism, which can't).
- **Scan-only fallback**: when `Command::new("dart").output()` fails (missing binary), `collect_dart_analyzer_symbol_scan` degrades to a `common_scan`-only mode where the oracle scan is built by re-parsing the same workspace files with tree-sitter-dart (the same code path the squeezy backend uses) — a degenerate self-compare that exercises the bench wiring without the analyzer. The report records `status: "Dart analyzer oracle unavailable; degraded to scan-only"` and the `mode: "scan-only"` field on the oracle report so gates branch on it (mirrors Ruby's path).

## 10. Gate thresholds (first PR)

`precision >= 0.93`, `recall >= 0.85`.

Justification: Dart is mostly statically typed and the analyzer's element model is authoritative — far less dynamic dispatch than Ruby/Python, fewer parse ambiguities than C++. The two recall-capping gaps are well-bounded: (a) `noSuchMethod` runtime dispatch (excluded symmetrically, so it doesn't count) and (b) codegen files (excluded via glob in the first PR, compared separately later). Precision matches Java's bar — parse-time symbols are unambiguous, and the only spurious-FP risk is misclassifying anonymous extensions or extension-on-dynamic-types as named classes, which the `Confidence::Partial` path already isolates. Wire into `gates.rs` alongside the Ruby block:

```rust
if !no_speed_gate
    && let Some(dart) = &report.dart_oracle
    && (dart.symbols.precision < 0.93 || dart.symbols.recall < 0.85)
{
    return Err(SqueezyError::Graph(format!(
        "Dart oracle accuracy regressed: precision={:.3} recall={:.3}",
        dart.symbols.precision, dart.symbols.recall
    )));
}
```

Use `<` against thresholds rather than `!= 0` on FP/FN counts so the codegen-edge cases (when the exclusion glob misses something) produce graceful margin rather than hard failures.

## 11. Speed parity target

Within **1.5x** of JS/TypeScript's per-file `parse + extract` time on the hand-built fixture (JS/TS is the closest analogue and shares Dart's mixed-static/dynamic visitor shape). Concretely: on a clean run of `benchmarks/fixtures/dart/semantic-cases/`, the per-file parse+extract wall time reported by the bench must be `<= 1.5 * js_ts_per_file_ms` on the same machine. Tracked in the smoke report; no hard CI gate in the first PR (so the part-of resolver and mixin ancestor walk can land without thrashing on noisy CI numbers), promoted to a hard gate once the resolver stabilises (matches Ruby's progression).

## 12. CI matrix entry

Append `dart` to the `language` choice list (lines ~33-41) and update the runner timeout gating logic on line 64 of `.github/workflows/benchmark-lang.yml`:

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
          - dart
```

Update the runner timeout expression to include Dart at 45 minutes (matches Ruby's smoke budget; the analyzer is fast and the fixture is small):

```yaml
    timeout-minutes: ${{ (inputs.language == 'c-family' || inputs.language == 'csharp') && 90 || inputs.language == 'go' && 60 || (inputs.language == 'ruby' || inputs.language == 'dart') && 45 || 120 }}
```

Add a Dart toolchain + oracle prep step before the benchmark run, immediately after `Setup benchmark`:

```yaml
      - name: Setup Dart 3.x (analyzer oracle)
        if: inputs.language == 'dart'
        continue-on-error: true
        uses: dart-lang/setup-dart@v1
        with:
          sdk: stable

      - name: Install Dart oracle helper deps
        if: inputs.language == 'dart'
        continue-on-error: true
        working-directory: benchmarks/oracle-helpers/dart-oracle
        run: dart pub get

      - name: Verify Dart analyzer availability
        if: inputs.language == 'dart'
        continue-on-error: true
        run: dart --version
```

`continue-on-error: true` on all three ensures the oracle degrades to scan-only mode (per §9) instead of failing the workflow if the Dart toolchain is unavailable. The corpus entry to add to `benchmarks/corpus.json`:

```json
{
  "name": "dart-smoke",
  "family": "dart",
  "language": "dart",
  "tier": "smoke",
  "fixture": "benchmarks/fixtures/dart/semantic-cases",
  "spec": "benchmarks/specs/dart-smoke-queries.json",
  "report": "dart/dart-smoke.json",
  "ra_lsp_probes": 25
}
```

A `dart-full` entry pointing at the `path_provider` checkout (`repo.url = https://github.com/flutter/packages`, `repo.rev = <verified-SHA>`, `checkout: target/benchmark-repos/path-provider-smoke`, `subdir: packages/path_provider/path_provider`) follows in the second PR once smoke is green, mirroring the Ruby `sinatra-full` progression.
