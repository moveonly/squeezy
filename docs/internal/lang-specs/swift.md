# Swift language implementation spec

Status: planning artifact for follow-up commit on branch `langs/swift`.
Scaffold already landed: `LanguageKind::Swift`, `LanguageFamily::Swift`, `.swift`
extension mapping, and placeholder `extract_swift` in
`crates/squeezy-parse/src/languages/swift.rs` that returns
`ParsedFile::unsupported(...)`.

This spec describes the full extractor, graph resolver, grammar dep, fixtures,
oracle plumbing, smoke spec, gate thresholds, and CI matrix entry needed to land
Swift support.

---

## 1. Template choice

Primary template: **Java** (`crates/squeezy-parse/src/languages/java.rs`).

Justification:

- Swift's top-level shape is class/struct/enum/protocol/extension declarations
  with member methods, fields, and computed properties — the same structural
  buckets Java's extractor already walks (class/interface/enum/record →
  methods/fields). The owner-symbol propagation in
  `visit_java_node` (parent symbol pair + owner symbol) maps cleanly to Swift
  nesting.
- `import_declaration` in Swift maps to Java's `import_declaration` semantics
  (named module import, no package keyword though — see gotcha (i)).
- Method invocation + member expression handling mirrors
  `extract_java_method_invocation` / `extract_java_object_creation`.
- Protocol conformance via `extension Foo: Bar { ... }` parallels Java's
  `implements` clause; reuse the `base:<Name>` attribute idiom from
  `java_type_inheritance_names`.

Secondary template: **C-family** (`crates/squeezy-parse/src/languages/c_family.rs`).

Justification:

- Top-level `func` / `struct` / `enum` at file scope (no enclosing class) needs
  the C-family pattern of "function declared at file scope → `SymbolKind::Function`,
  function declared inside type-owning parent → `SymbolKind::Method`" (see
  `c_family_symbol_can_own_members`). Swift permits free functions at module
  scope, so we cannot mechanically apply Java's "everything inside a class is a
  Method" assumption.
- `c_family_call_is_macro_like` and the macro-opaque confidence treatment are
  the right shape for Swift's `@dynamicMemberLookup` and macros (`#externalMacro`).

**Resolution:** new file `crates/squeezy-parse/src/languages/swift.rs` containing
a `visit_swift_node` walker that copies the structure of `visit_java_node` and
imports the file-scope-vs-member kind selection from c_family. Reuse the shared
helpers in `crate::languages::rust::*` (signature_text, span_from_node,
node_text, named_child_count, symbol_id, extract_body_hit, last_path_segment).

A corresponding `crates/squeezy-graph/src/languages/swift.rs` mirrors
`crates/squeezy-graph/src/languages/java.rs`: `swift_module_for_file`,
`swift_import_matches_symbol`, `swift_symbol_owner_path`, and the extension-aware
receiver-method resolver described in gotcha (a).

---

## 2. Grammar

Recommendation: `tree-sitter-swift` published to crates.io by
[`alex-pinkus/tree-sitter-swift`](https://github.com/alex-pinkus/tree-sitter-swift)
— the most-maintained Swift grammar in the OSS community as of 2026-05.

**Verify on crates.io before pinning.** As of writing, the most-known published
crate version is `tree-sitter-swift = "0.6"` (or `"0.7"` — check
`https://crates.io/crates/tree-sitter-swift` for the latest tagged release
compatible with `tree-sitter = "0.25"` (the version this workspace uses; see
`crates/squeezy-parse/Cargo.toml`)). If only an older crate is on crates.io
(e.g. 0.4 with `tree-sitter = "0.20"`), fall back to a git rev pin:

```toml
[workspace.dependencies]
tree-sitter-swift = { git = "https://github.com/alex-pinkus/tree-sitter-swift", rev = "<sha-of-tagged-release>" }
```

Pin a tagged release SHA (not `HEAD`) so reproducible builds are preserved.

Wire-up follows the Java pattern in `crates/squeezy-parse/src/lib.rs`:

- Add `swift_parser: Parser` field to `LanguageParser` (~line 228) and
  `WorkerParsers` (~line 498).
- Add `swift_parser: parser_with_swift_language()?` to `LanguageParser::new`
  (~line 261).
- Add `LanguageKind::Swift => Ok(&mut self.swift_parser)` to
  `parser_for_language` (~line 459).
- Add `LanguageKind::Swift => parser_with_swift_language()` to
  `parser_for_language_kind` (~line 479).
- Add `fn swift_language() -> tree_sitter::Language { tree_sitter_swift::LANGUAGE.into() }`
  alongside `java_language` (~line 766).
- Add `LanguageKind::Swift => Some(swift_language())` to `language_for_kind`
  (~line 798); remove Swift from the unsupported arm there.
- Add `fn parser_with_swift_language() -> Result<Parser> { ... }` mirroring
  `parser_with_java_language`.
- Wire `extract_swift` into `extract_language` (whichever match arm dispatches
  to `extract_java` today).
- Replace the placeholder body of `crates/squeezy-parse/src/languages/swift.rs`
  with the full extractor.

Note: tree-sitter-swift's grammar exposes the upstream node names directly
(`class_declaration`, `protocol_declaration`, etc.) without a separate "language
function" naming scheme. Confirm the crate exports a `LANGUAGE` const (newer
tree-sitter convention) vs `language()` fn (older) before wiring.

---

## 3. AST-node → fact mapping

Node names below follow alex-pinkus/tree-sitter-swift's grammar. Field-name
references (`name`, `body`, `parameters`) follow the same grammar's
`grammar.js`. Verify exact names against the installed crate version — minor
versions have renamed `function_declaration` ↔ `function_signature` at points
in the past.

| Tree-sitter node                              | Squeezy fact                                                              | `SymbolKind`        | Notes                                                                            |
|-----------------------------------------------|---------------------------------------------------------------------------|---------------------|----------------------------------------------------------------------------------|
| `import_declaration`                          | `ParsedImport { kind: Named, path: "<module>" }`                          | —                   | `Foundation`, `SwiftUI`. No glob form. See gotcha (i).                            |
| `class_declaration`                           | `ParsedSymbol`                                                            | `Class`             | Inheritance/conformance via `inheritance_clause` field → emit `base:<Name>` attributes. |
| `struct_declaration`                          | `ParsedSymbol`                                                            | `Struct`            | Conformance → `base:<Protocol>` attributes.                                       |
| `actor_declaration`                           | `ParsedSymbol`                                                            | `Class`             | Add attribute `swift:actor`. See gotcha (d).                                      |
| `protocol_declaration`                        | `ParsedSymbol`                                                            | `Trait`             | Maps to Squeezy `Trait` (same kind Java uses for `interface_declaration`).        |
| `enum_declaration`                            | `ParsedSymbol`                                                            | `Enum`              | Associated-value cases → see `enum_case_declaration` row.                         |
| `enum_case_declaration` / `enum_entry`        | `ParsedSymbol` per case                                                   | `Variant`           | One per name in the case clause (`case foo, bar` → two symbols).                  |
| `extension_declaration`                       | Synthetic owner; members emit with `language_identity = "<ExtendedType>"` | (no symbol)         | See gotcha (a). Conformance via `inheritance_clause` → record reference on extended type. |
| `function_declaration`                        | `ParsedSymbol`                                                            | `Function`/`Method` | File scope → `Function`; inside `class`/`struct`/`enum`/`actor`/`protocol`/`extension` → `Method`. Same selection rule as C-family. |
| `init_declaration`                            | `ParsedSymbol`, name = `"init"`                                           | `Method`            | Attribute `swift:init`. Convenience/required → additional attributes.             |
| `deinit_declaration`                          | `ParsedSymbol`, name = `"deinit"`                                         | `Method`            | Attribute `swift:deinit`.                                                         |
| `subscript_declaration`                       | `ParsedSymbol`, name = `"subscript"`                                      | `Method`            | Attribute `swift:subscript`. Synthetic name; the grammar doesn't surface a name field. |
| `property_declaration`                        | `ParsedSymbol`                                                            | `Field`             | Stored or computed (see gotcha (e)). One symbol per name in the pattern, like `java_field_symbols_from_node`. |
| `typealias_declaration`                       | `ParsedSymbol`                                                            | `TypeAlias`         |                                                                                  |
| `call_expression`                             | `ParsedCall`                                                              | —                   | Receiver from `child_by_field_name("function")` chain; same shape as C-family.    |
| `navigation_expression` / `member_expression` | `ParsedReference { kind: Field }` (member access) or `Path`               | —                   | Source for `body_hits` of kind `Identifier`/`Path`.                               |
| `type_identifier`                             | `ParsedReference { kind: Type }`                                          | —                   | Skip when it's the declaration name (parent's `name` field — same guard as `c_family_node_is_declaration_name`). |
| `user_type` / `simple_user_type`              | `ParsedReference { kind: Type }`                                          | —                   | Emit `Type` references for generic constraints and conformance lists.             |
| `attribute`                                   | `ParsedReference { kind: Attribute }`                                     | —                   | `@Published`, `@MainActor`, `@objc`, `@Sendable` → attribute on host symbol.      |
| `inheritance_clause`                          | One `ParsedReference { kind: Type }` per listed type                       | —                   | Drives `base:<Name>` attributes on the owner symbol.                              |
| `string_literal` / `integer_literal` / `boolean_literal` / `nil` | `BodyHit { kind: Literal }`                            | —                   | Same treatment as `is_c_family_literal`.                                          |

Things NOT emitted as symbols:

- `closure_expression` (locals — see gotcha (l))
- Computed-property `getter_specifier` / `setter_specifier` (see gotcha (e))
- String-interpolation `\(expr)` body (body-hit only — see gotcha (k))
- `if let` / `guard let` binders (locals)
- Parameter labels (already part of the function's arity/signature text)

---

## 4. Language gotchas & heuristics

### (a) Extensions

```swift
extension Foo {
    func bar() { ... }
}
```

`extension_declaration` is NOT itself a symbol (no Squeezy `SymbolKind::Impl`
analog for Swift — Java's resolver doesn't synthesize a parent for `class Foo {}`
either, and Swift extensions can live in any file). Instead, when visiting an
`extension_declaration`:

1. Resolve the extended type name from `child_by_field_name("name")` (or first
   `type_identifier` child) → e.g. `"Foo"`.
2. For each member declaration inside the extension body, emit the symbol with
   `parent_id = None` and `language_identity = Some("Foo")`. This mirrors how
   Swift treats extension members as members of `Foo` for dispatch.
3. In the graph resolver (`crates/squeezy-graph/src/languages/swift.rs`), the
   receiver-method lookup checks both `symbols.iter().filter(|s| s.parent.name ==
   receiver_type)` AND `symbols.iter().filter(|s| s.language_identity.as_deref()
   == Some(receiver_type))`. This makes `foo.bar()` resolve to the extension's
   `bar` even when `foo: Foo` is declared in a different file from the extension.

This is the same mechanism C# uses for partial classes (`PartialOf` edges), and
Java uses `language_identity` for nested-class hoisting in some paths. The
`language_identity` field on `ParsedSymbol` already exists in the schema
(see `crates/squeezy-parse/src/lib.rs:72`); we're just adopting it here.

### (b) Protocol conformance

```swift
extension Foo: Bar { ... }
class Baz: SomeClass, Bar, Baz { ... }
```

For both extension and direct declarations, walk the `inheritance_clause` and
emit one `ParsedReference { kind: Type, text: "<Name>" }` per listed type, with
`owner_id = Some(owner_symbol.id)`. Then add `base:<Name>` attributes to the
host symbol — same pattern as `java_type_inheritance_names` in
`crates/squeezy-parse/src/languages/java.rs:175-180`.

Heuristic for extensions adding conformance retroactively: emit a reference on
the extended type's *first definition we've seen* if it's in our index; otherwise
just record the reference floating on the extension's first member. The graph
resolver does the after-the-fact attachment when it indexes both.

### (c) Property wrappers

```swift
@Published var x: Int = 0
```

- Emit `x` as `SymbolKind::Field` (matching how Swift treats property wrappers
  semantically — wrapped storage is still a property).
- Record `@Published` as `ParsedReference { kind: Attribute, text: "Published" }`
  with `owner_id` pointing at `x`. Strip the leading `@`.
- Do NOT model the wrapper-synthesized `$x` projected value or `_x` storage —
  those are compiler-internal and would inflate FP against SourceKit-LSP.

### (d) Actors

`actor Foo { ... }` → `SymbolKind::Class` with attributes `swift:actor` and
(if applicable) `base:<Conformance>`. Distinguished from regular classes only
by the attribute, since actors do not change the symbol-search shape — they
just affect call-site isolation, which is out of scope for syntactic
extraction.

### (e) Computed properties

```swift
var fullName: String {
    get { "\(first) \(last)" }
    set { last = newValue }
}
```

- Emit `fullName` as `SymbolKind::Field` (NOT `Method`). Swift IDE tooling
  treats computed and stored properties uniformly as properties.
- The `get` and `set` accessor blocks are NOT separate symbols. Body-hit the
  identifiers inside them with `owner_id = Some(fullName.id)`.
- Add attribute `swift:computed` so downstream queries can distinguish stored
  vs computed without re-parsing.
- Confidence: `Partial` if the getter body invokes a receiver we cannot resolve
  in the same file (heuristic).

### (f) `@MainActor` / `@Sendable` / `@objc` / `@available`

These are all `attribute` nodes in the grammar. Treat them as
`ParsedReference { kind: Attribute }` and `BodyHit { kind: Attribute }` on the
host symbol, identical to Java's `extract_java_annotation_reference`. Strip the
leading `@` so the text matches the bare type name.

`@objc(customName)` → record both `objc` as the attribute and `customName` as a
second `Attribute` reference for searchability.

### (g) Generics with constraints

```swift
func foo<T: Codable, U>(_: T, _: U) where U: Equatable { ... }
```

- Emit `foo` as the function symbol; its `arity` is 2 (positional parameter
  count).
- For each constraint in the generic clause (`T: Codable`, `U: Equatable`),
  emit a `ParsedReference { kind: Type, text: "Codable" }` with
  `owner_id = Some(foo.id)`. Captures both clause types and `where`-clause
  types.

### (h) `@dynamicMemberLookup`

A type marked `@dynamicMemberLookup` accepts arbitrary `.foo` member access
that the compiler routes to `subscript(dynamicMember:)`. We have no way to
statically resolve those at parse time.

- On the type declaration, record `@dynamicMemberLookup` as an attribute.
- Member-access references inside instances of dynamic-lookup types emit at
  `Confidence::Partial`. Practically: when the receiver is a known
  `@dynamicMemberLookup` type, override the default `Confidence::CandidateSet`
  to `Confidence::Partial` for member-access body hits.

### (i) Module imports

```swift
import Foundation
import SwiftUI
import struct CoreGraphics.CGRect
```

- `import Foundation` → `ParsedImport { kind: Named, path: "Foundation",
  alias: None, is_glob: false }`. Mark `imported_name = Some("Foundation")`.
  Distinct from Java's package model: Swift has no `package` declaration to
  emit a `__swift_module__` synthetic import for. The compiler infers module
  from the SwiftPM `Sources/<ModuleName>/` directory.
- Synthesize a "module" hint by walking the file path: if `relative_path`
  matches `Sources/<X>/...`, store `package = Some("<X>")` on the `ParsedFile`
  so cross-file resolution can use the module name. This replaces Java's
  `package_declaration` flow. Otherwise `package = None`.
- `import struct CoreGraphics.CGRect` (kind-filtered import) → still
  `ParsedImport { kind: Named, path: "CoreGraphics.CGRect",
  imported_name: Some("CGRect") }`. Drop the leading kind keyword.

### (j) `@objc`-bridged Objective-C

Swift code can use `@objc` to expose APIs to Objective-C and consume bridged
Objective-C symbols. We do NOT parse `.h`/`.m` headers in this PR.

- If a Swift file contains `import <FrameworkName>` where the framework is
  Objective-C-only (e.g. `UIKit`), we emit the import like any other but
  cross-references into UIKit will not resolve. Flag with a `swift:objc-bridge`
  diagnostic for that import only.
- `@objc(name)` attributes are recorded (gotcha (f)) but the bridging mapping
  is out of scope.

### (k) String interpolation

```swift
let msg = "Hello \(name), you owe \(amount)"
```

- The literal nodes around the interpolation are `string_literal`.
- The grammar exposes the inner expressions as `interpolated_expression` (or
  similar — verify exact name) named children. Walk into them and treat them
  as ordinary expressions for body-hit/reference extraction.
- Do NOT emit the interpolation segments as separate symbols.

### (l) Closures

```swift
let mapper: (Int) -> Int = { $0 * 2 }
items.map { $0.uppercased() }
```

- `closure_expression` body is treated as a continuation of the enclosing
  symbol's body. Body-hit `$0` member access etc. onto the enclosing owner.
- Do NOT emit closures as their own symbols. The Squeezy schema treats them as
  locals — consistent with how `closure_expression` is handled in
  `crates/squeezy-parse/src/languages/rust.rs`.

---

## 5. Per-symbol confidence rules

Follow the C-family `c_family_symbol_confidence` template
(`crates/squeezy-parse/src/languages/c_family.rs:960-983`):

| Situation                                                                                     | Confidence              |
|-----------------------------------------------------------------------------------------------|-------------------------|
| Static declaration (class, struct, enum, protocol, actor, function, init, deinit, typealias, stored property) | `Confidence::ExactSyntax` |
| Computed property whose getter body invokes an unresolved receiver                            | `Confidence::Partial`    |
| Member access on a `@dynamicMemberLookup` receiver                                            | `Confidence::Partial`    |
| Protocol witness method whose conforming type is not visible in the same file (heuristic: extension in a different file from the protocol declaration and the conforming type) | `Confidence::Partial`    |
| Member declared inside an extension where the extended type is not yet indexed               | `Confidence::Heuristic`  |
| `@dynamicCallable` types' call sites                                                          | `Confidence::Partial`    |
| Macro invocation (`#externalMacro(...)`, `#freestanding(...)`)                                | `Confidence::MacroOpaque` |
| Conditional compilation (`#if`, `#elseif`, `#endif`) — symbol inside a branch                | `Confidence::ConditionalUnknown` |
| Parse-tree contains `has_error()` for the symbol's subtree                                    | Existing diagnostic at `Confidence::Partial` |

`ParsedCall` confidence defaults:

- Direct call with no receiver → `Confidence::Heuristic` (same as C-family).
- Method call (has receiver) → `Confidence::CandidateSet`.
- All-caps macro-like name → `Confidence::MacroOpaque`.

---

## 6. Fixture sketch

Layout: `benchmarks/fixtures/swift/semantic-cases/` mirrors
`benchmarks/fixtures/csharp/semantic-cases/`.

Recommended fixture set (4–6 source files plus excluded directories):

```
benchmarks/fixtures/swift/semantic-cases/
├── Package.swift                                   # SwiftPM manifest, fallback metadata
├── Sources/
│   ├── Networking/
│   │   └── Endpoint.swift                          # protocol + conforming struct + extension
│   ├── Storage/
│   │   └── Cache.swift                             # actor declaration, async methods
│   ├── Extensions/
│   │   └── String+Sanitize.swift                   # cross-file extension on String
│   └── Models/
│       └── Result.swift                            # generic with `where` constraint, enum
├── vendor/
│   └── SwiftBundled/
│       └── Bundled.swift                           # synthetic SwiftPM dep — fallback exclusion
└── generated/
    └── R.generated.swift                           # SwiftGen-style output — fallback exclusion
```

**Per-file content sketches:**

- `Sources/Networking/Endpoint.swift`:
  - `import Foundation`
  - `protocol Endpoint { var path: String { get }; func encode() -> Data }`
  - `struct UserEndpoint: Endpoint { let path = "/users"; func encode() -> Data { ... } }`
  - Tests: protocol → conforming struct resolution, computed property field
    classification, signature search for `func encode()`.

- `Sources/Storage/Cache.swift`:
  - `import Foundation`
  - `actor Cache<Key: Hashable, Value> { private var storage: [Key: Value] = [:];
    func get(_ key: Key) async -> Value? { ... }; func set(_ key: Key, _ value: Value) async { ... } }`
  - Tests: actor classification, generic constraint capture, async method
    extraction.

- `Sources/Extensions/String+Sanitize.swift`:
  - `import Foundation`
  - `extension String { func sanitized() -> String { trimmingCharacters(in: .whitespaces) } }`
  - Tests: extension method registered with `language_identity = "String"`,
    cross-file call resolution to a `String`-typed receiver from another
    fixture file.

- `Sources/Models/Result.swift`:
  - `enum APIResult<Value, Failure: Error> { case success(Value); case failure(Failure); func map<New>(_ transform: (Value) -> New) -> APIResult<New, Failure> { ... } }`
  - Tests: enum variant extraction, generic constraint `Failure: Error`,
    method with own generic parameter on enum.

- (Optional 5th) `Sources/Networking/Repository.swift`:
  - `@MainActor final class UserRepository { @Published var users: [String] = [];
    func refresh() async { ... } }`
  - Tests: `@MainActor` and `@Published` attribute capture, computed-property vs
    stored-property classification.

- `vendor/SwiftBundled/Bundled.swift`: any Swift content; fixture asserts the
  whole `vendor/` tree is excluded from oracle comparisons via
  `OracleExclusions`.

- `generated/R.generated.swift`: similar; assertion that filenames matching
  `*.generated.swift` are excluded.

---

## 7. Real-repo corpus entry

Recommendation: [`apple/swift-nio`](https://github.com/apple/swift-nio),
`NIOCore` module.

| Field                | Value                                                   |
|----------------------|---------------------------------------------------------|
| Repo URL             | `https://github.com/apple/swift-nio`                    |
| Suggested tag        | `2.65.0` (or latest 2.x at PR time; pin the SHA)        |
| Smoke fixture path   | `target/benchmark-repos/swift-nio/Sources/NIOCore`     |
| Smoke subset (~80 files) | `Sources/NIOCore/*.swift`                         |
| `mixed_iterations`   | 1500 (matches csharp corpus entries)                    |
| `ra_lsp_probes`      | 25 for smoke, 50 for full (matches js-ts pattern)       |
| Why                  | Idiomatic modern Swift, pure-Swift (no Obj-C bridging), widely used, ~250 source files in `NIOCore` alone, exercises generics + protocols + extensions heavily. |

Optional second `full`-tier entry: `pointfreeco/swift-composable-architecture`
(`TCA`) — heavier use of `@dynamicMemberLookup`, property wrappers, and result
builders; useful as a stress test once smoke gates pass.

Corpus JSON entry shape (add to `benchmarks/corpus.json`):

```json
{
  "name": "swift-smoke",
  "family": "swift",
  "language": "swift",
  "tier": "smoke",
  "fixture": "benchmarks/fixtures/swift/semantic-cases",
  "spec": "benchmarks/specs/swift-smoke-queries.json",
  "report": "swift/swift-smoke.json",
  "ra_lsp_probes": 25,
  "no_speed_gate": true
},
{
  "name": "swift-nio",
  "family": "swift",
  "language": "swift",
  "tier": "full",
  "fixture": "target/benchmark-repos/swift-nio/Sources/NIOCore",
  "spec": "benchmarks/specs/swift-smoke-queries.json",
  "report": "swift/swift-nio.json",
  "mixed_repo": "target/benchmark-repos/swift-nio/Sources/NIOCore",
  "mixed_iterations": 1500,
  "ra_lsp_probes": 25,
  "no_speed_gate": true,
  "repo": {
    "url": "https://github.com/apple/swift-nio",
    "rev": "<pin-sha-of-2.65.0>",
    "checkout": "target/benchmark-repos/swift-nio"
  }
}
```

`no_speed_gate: true` for both entries in the first PR. The speed gate compares
against the oracle, and SourceKit-LSP's first-run indexing dominates wall time
in ways that aren't representative of steady-state IDE use.

---

## 8. Smoke query spec

Write to `benchmarks/specs/swift-smoke-queries.json`. Targets symbols defined
in the fixtures in section 6.

```json
{
  "queries": [
    {
      "id": "swift-declarations",
      "kind": "signature_search",
      "text": "",
      "expected_contains": [
        "Trait:Endpoint",
        "Struct:UserEndpoint",
        "Class:Cache",
        "Class:UserRepository",
        "Enum:APIResult",
        "Method:encode",
        "Method:get",
        "Method:set",
        "Method:sanitized",
        "Method:map",
        "Method:refresh",
        "Field:path",
        "Field:users",
        "Variant:success",
        "Variant:failure"
      ]
    },
    {
      "id": "swift-attributes",
      "kind": "signature_search",
      "text": "",
      "attribute": "swift:actor",
      "expected_contains": [
        "Class:Cache"
      ]
    },
    {
      "id": "swift-mainactor-attribute",
      "kind": "signature_search",
      "text": "",
      "attribute": "MainActor",
      "expected_contains": [
        "Class:UserRepository"
      ]
    },
    {
      "id": "swift-protocol-references",
      "kind": "references_to_symbol",
      "to": "Endpoint",
      "expected_contains": [
        "Endpoint"
      ]
    },
    {
      "id": "swift-extension-call-chain",
      "kind": "call_chain",
      "from": "refresh",
      "to": "sanitized",
      "expected_contains": [
        "refresh -> sanitized"
      ]
    },
    {
      "id": "swift-actor-method-references",
      "kind": "references_to_symbol",
      "to": "Cache.get",
      "expected_contains": [
        "Cache.get"
      ]
    },
    {
      "id": "swift-generic-constraint-edges",
      "kind": "edges",
      "expected_contains": [
        "Implements:UserEndpoint->Endpoint:Endpoint:Heuristic"
      ]
    },
    {
      "id": "swift-body-search",
      "kind": "body_search",
      "text": "trimmingCharacters",
      "expected_contains": [
        "sanitized:trimmingCharacters"
      ]
    },
    {
      "id": "swift-swiftpm-facts",
      "kind": "swift_project_facts",
      "expected_contains": [
        "package-swift:metadata_file:Package.swift",
        "swiftpm:module:Networking",
        "swiftpm:module:Storage",
        "swiftpm:module:Models",
        "swiftpm:module:Extensions"
      ]
    },
    {
      "id": "swift-fallback-quality",
      "kind": "fallback_quality",
      "expected_contains": [
        "generated",
        "vendor"
      ]
    }
  ]
}
```

The `swift_project_facts` query kind is new; implement as a new arm in the
benchmark's query dispatcher analogous to `dotnet_project_facts`. Optional for
the first PR — can ship with only the cross-language query kinds and add SwiftPM
facts as a follow-up.

---

## 9. Oracle plan

### Tool

**SourceKit-LSP** — the official LSP server bundled with Apple's open-source
Swift toolchains (`https://github.com/swiftlang/sourcekit-lsp`). Provides full
type checking, symbol/definition/reference navigation, and matches how
`rust_analyzer.rs` drives rust-analyzer. Binary is named `sourcekit-lsp` and
ships inside every Swift toolchain at `usr/bin/sourcekit-lsp`.

Justification:

- Same LSP protocol as rust-analyzer → the existing LSP plumbing in
  `crates/squeezy-graph/benchmarks/squeezy-graph-bench/src/oracles/rust_analyzer.rs`
  (`RustAnalyzerLsp`, `request`, `notify`, `parse_lsp_locations`) is directly
  reusable.
- Authoritative oracle: covers protocol conformances, extension members,
  generic constraints, actor isolation — everything the syntactic extractor
  approximates.

Alternative considered & rejected for first PR: `swift symbolgraph-extract`.
Produces JSON symbol graphs but only operates on already-built SwiftPM modules,
which adds a full `swift build` step per fixture and is much slower than LSP
queries.

### Helper layout

New file: `benchmarks/squeezy-graph-bench/src/oracles/swift_sourcekit.rs`.

Refactor first: extract the LSP transport, lifecycle, and message-parsing
machinery currently inlined in `rust_analyzer.rs` into a new
`benchmarks/squeezy-graph-bench/src/oracles/lsp_oracle.rs` helper:

- `struct LspOracle { root, child, stdin, stdout, next_id, opened }`
- `LspOracle::start(program, language_id, root, init_capabilities)`
- `LspOracle::definition`, `LspOracle::references`, `LspOracle::did_open`,
  `LspOracle::request`, `LspOracle::notify`, `LspOracle::write_message`,
  `LspOracle::read_message`, `Drop` shutdown

Then both `rust_analyzer.rs` and `swift_sourcekit.rs` become thin façades that
parametrize `LspOracle` with the binary name and language id (`"rust"` vs
`"swift"`). This is a separate, dependency-free refactor that should land in
the same PR.

`swift_sourcekit.rs` exports:

- `swift_sourcekit_lsp_program() -> Option<String>` — `which sourcekit-lsp` or
  read `SOURCEKIT_LSP` env var override.
- `collect_swift_sourcekit_symbol_scan(graph: &SemanticGraph) -> (SymbolScan, String)`
  — workspace/documentSymbol scan, mirrors
  `collect_rust_analyzer_symbol_scan`.
- `SwiftSourceKitLsp` type implementing `definition` / `references` via the
  shared `LspOracle`.

No sidecar process is required — `sourcekit-lsp` is the only subprocess.

### Install in CI

- **Linux runner** (Ubuntu 22.04): install Swift 5.10 from `swift.org`. Easiest
  path is the `swiftlang/swift-action` GitHub Action or a direct tarball:

  ```yaml
  - name: Install Swift toolchain
    run: |
      SWIFT_VERSION=5.10
      SWIFT_PLATFORM=ubuntu22.04
      curl -fL -o swift.tar.gz \
        https://download.swift.org/swift-${SWIFT_VERSION}-release/${SWIFT_PLATFORM//./}/swift-${SWIFT_VERSION}-RELEASE/swift-${SWIFT_VERSION}-RELEASE-${SWIFT_PLATFORM}.tar.gz
      sudo tar -xzf swift.tar.gz -C /usr/local --strip-components=1
      swift --version
      sourcekit-lsp --version || true
  ```

- **macOS runner**: `sourcekit-lsp` ships with Xcode 15+. Use
  `xcode-select -p` and `xcrun -f sourcekit-lsp` to locate it.

Pin to **Swift 5.10** for the first PR. Swift 6 is recent enough that some
features (typed throws, full strict concurrency by default) may surface
extractor edge cases we haven't characterized yet.

### Scan strategy

- **Symbol scan**: `workspace/symbol` (empty query) + per-file
  `textDocument/documentSymbol` for every Swift file in the graph. Aggregate
  declaration counts into `SymbolScan.counts` keyed by
  `SymbolKey { file, kind, name }`, with kind normalized via a new
  `normalize_sourcekit_lsp_kind` function in the shared LSP helper.
- **Navigation probes**: `textDocument/definition` and `textDocument/references`
  at sampled identifier offsets. Set `ra_lsp_probes: 25` in the smoke corpus
  entry (carry forward the field name; the bench code calls it
  `ra_lsp_probes` for historical reasons but the value is reused for any LSP
  oracle).
- Wire into `collect_navigation_accuracy` in
  `benchmarks/squeezy-graph-bench/src/accuracy.rs` by introducing a new
  `Oracle::SwiftSourceKit` variant for the language match arm.

### Exclusion list

The oracle should ignore declarations that Squeezy intentionally does NOT
synthesize:

- Locals (parameters, `let`/`var` bindings inside function bodies, closure
  captures).
- Closures.
- Computed-property `get`/`set` accessors (Squeezy emits one Field, not three
  symbols).
- `@dynamicMemberLookup` synthetic members (`$foo` projected values, dynamic
  member accesses).
- Files matching `*.generated.swift` (SwiftGen, Sourcery, etc.).
- Files inside `vendor/` and any directory listed in the fixture's
  `OracleExclusions` from `default_oracle_exclusions`.

Encode the kind-level filter in the new SourceKit `normalize_sourcekit_lsp_kind`
(returns `None` for `Variable` when the LSP kind says
`SymbolKind::Property` with the `accessor:true` marker, etc.).

### Definition/reference probes

Yes — wire fully into `collect_navigation_accuracy`. The same sampling logic
that rust-analyzer uses (random N=`ra_lsp_probes` identifier offsets per file,
compare Squeezy's resolution against the LSP's) is directly reusable.

### Scan-only fallback

If `sourcekit-lsp` is not on PATH and `SOURCEKIT_LSP` env var is unset,
degrade gracefully to the existing `collect_squeezy_symbol_scan` against the
common-scan oracle (`benchmarks/squeezy-graph-bench/src/oracles/common_scan.rs`)
and emit a status message — same pattern as
`rust_analyzer_program()` returning `None`. This keeps the benchmark runnable
locally on machines without Swift installed.

---

## 10. Gate thresholds for first PR

| Metric                          | Threshold | Notes |
|---------------------------------|-----------|-------|
| Symbol precision                | `>= 0.92` | Slightly lower than C# (`>= 0.93` in practice). Swift extension members emitted with `language_identity` will count as TP against SourceKit, but extension members spread across many files may temporarily inflate FP before resolver attachment. |
| Symbol recall                   | `>= 0.80` | Computed-property accessor merging and protocol witness attachment cause ~10–15% systematic divergence vs the LSP. |
| Definition probe precision      | `>= 0.75` | Probe-based, sampled — same shape as rust-analyzer probes. |
| Definition probe recall         | `>= 0.65` | |
| Reference probe precision       | `>= 0.70` | |
| Reference probe recall          | `>= 0.55` | |
| Missing-results gate            | `0`       | All `expected_contains` items in the smoke spec must hit (mandatory; see `enforce_gates`). |
| Speed gate                      | disabled (`no_speed_gate: true`) | Re-enable in a follow-up PR once SourceKit-LSP timing baselines are characterized in CI. |

These match the gate shape in `benchmarks/squeezy-graph-bench/src/gates.rs`,
which enforces "missing expected" and the speed comparison only — symbol/nav
thresholds are tracked in the report and reviewed manually for the first PR.
Add Swift-specific assertions to `gates.rs` once thresholds stabilize.

---

## 11. Speed parity target

Parse + extract: **within 1.5× of C-family** per-file wall time on the smoke
fixture. Rationale:

- tree-sitter-swift is heavier than tree-sitter-c (Swift's grammar is larger
  than C's by ~3×) but lighter than tree-sitter-scala.
- The Java extractor's visitor pattern is the lower-bound complexity target;
  the Swift extractor adds (a) extension `language_identity` propagation, (b)
  inheritance clause walking on every type. Both are constant-factor.
- Smoke fixture target: < 200ms total for ~6 files on an M-class macOS runner;
  < 500ms on the Ubuntu CI runner.

Confirm in the first PR via the existing `BenchmarkReport.squeezy_total_ms`
field; record but do not enforce as a hard gate.

---

## 12. CI matrix entry

Append to `.github/workflows/benchmark-lang.yml`. The reusable workflow
already accepts `language` as an input; add `swift` to the choice list, then
add a setup step gated on `inputs.language == 'swift'`.

```yaml
# In .github/workflows/benchmark-lang.yml, under workflow_dispatch.inputs.language.options:
        options:
          - rust
          - python
          - java
          - go
          - c-family
          - csharp
          - js-ts
          - swift

# Under jobs.benchmark.steps, BEFORE the "Run semantic graph benchmark corpus" step:
      - name: Install Swift toolchain
        if: inputs.language == 'swift'
        continue-on-error: true
        run: |
          set -e
          SWIFT_VERSION=5.10
          SWIFT_PLATFORM=ubuntu22.04
          ARCHIVE="swift-${SWIFT_VERSION}-RELEASE-${SWIFT_PLATFORM}.tar.gz"
          curl -fL -o swift.tar.gz \
            "https://download.swift.org/swift-${SWIFT_VERSION}-release/${SWIFT_PLATFORM//./}/swift-${SWIFT_VERSION}-RELEASE/${ARCHIVE}"
          sudo mkdir -p /opt/swift
          sudo tar -xzf swift.tar.gz -C /opt/swift --strip-components=1
          echo "/opt/swift/usr/bin" >> "$GITHUB_PATH"
          /opt/swift/usr/bin/swift --version
          /opt/swift/usr/bin/sourcekit-lsp --version || true
```

Wrap the Swift toolchain install step in `continue-on-error: true` for the
first PR so a transient swift.org outage doesn't red-X the run; the bench
falls back to `common_scan` automatically when `sourcekit-lsp` is unavailable.

Update the `runs-on` selector at line 63 of `benchmark-lang.yml` to keep
`ubuntu-latest` for Swift (Linux-only is acceptable for the first PR):

```yaml
runs-on: ${{ inputs.language == 'rust' && 'macos-latest' || 'ubuntu-latest' }}
```

(No change needed — Swift already routes to `ubuntu-latest` by the default
arm.)

**macOS-only follow-up**: some Swift framework-dependent test harnesses
(`Combine`, `SwiftUI`, `Network`) require macOS or iOS SDKs that ship only with
Xcode. The smoke fixture in section 6 is intentionally framework-light
(`Foundation` only) so it works on Linux. Note in the PR description that any
future fixture importing `Combine`/`SwiftUI` would require switching the
matrix arm to `macos-latest` for that case.

The timeout-minutes ladder at line 64 can stay as the 120-minute default arm
for Swift; no special timeout needed.

Update `.github/workflows/benchmark-lang.yml` `workflow_dispatch.inputs.language.options`
and the parent caller workflow (typically `.github/workflows/benchmarks.yml` or
similar — check before PR) so `swift` is reachable from the manual trigger UI.

---

## Implementation order for the follow-up PR

1. Refactor LSP plumbing out of `rust_analyzer.rs` into `lsp_oracle.rs`. No
   behavior change.
2. Add `tree-sitter-swift` dep, wire `parser_with_swift_language`, register in
   `LanguageParser` and `WorkerParsers`.
3. Implement `extract_swift` in
   `crates/squeezy-parse/src/languages/swift.rs` following the Java template
   + the AST mapping in section 3.
4. Add `crates/squeezy-graph/src/languages/swift.rs` with module-path and
   extension-aware resolver helpers.
5. Add fixtures under `benchmarks/fixtures/swift/semantic-cases/`.
6. Add smoke spec at `benchmarks/specs/swift-smoke-queries.json`.
7. Add `swift_sourcekit.rs` oracle, wire into `accuracy.rs`.
8. Add corpus entries for `swift-smoke` and `swift-nio`.
9. Update `.github/workflows/benchmark-lang.yml` per section 12.
10. Add tests in `crates/squeezy-parse/src/languages/swift/tests.rs` (mirror
    `crates/squeezy-parse/src/languages/java/tests.rs` shape — fixture →
    extract → snapshot symbols/imports/calls).
