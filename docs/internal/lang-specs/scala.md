# Scala 3 — Language Implementation Spec

Target branch: `langs/scala`. Scaffold already lives on `main`:
`LanguageKind::Scala` / `LanguageFamily::Scala`, `.scala`/`.sc` extension
mapping, `crates/squeezy-parse/src/languages/scala.rs::extract_scala` returning
`ParsedFile::unsupported(...)`, and corresponding `ScalaBackend` /
`ScalaGraphExt` entries in `crates/squeezy-parse/src/backend.rs` and
`crates/squeezy-graph/src/backend.rs`. This document covers the follow-up
implementation only.

---

## 1. Template choice

**Closest existing language: Java.** Justification:

- Both target the JVM, so the orchestrator's build-metadata + classpath +
  generated-sources story already in `java_build_metadata_provider`,
  `java_source_root_facts`, and `java_dependency_facts`
  (`crates/squeezy-graph/src/languages/java.rs:252-490`) ports directly. SBT,
  Mill, and Maven are the only Scala-specific bits.
- Package/import semantics are nearly identical to Java: dotted package
  declarations rooted at the file top, `import a.b.C` and `import a.b.*` (in
  Scala 3, `*` replaces Scala 2's `_`). The Java path-segment matcher in
  `java_import_matches_symbol`
  (`crates/squeezy-graph/src/languages/java.rs:8-65`) is the correct shape;
  Scala only adds aliasing (`{c => d}` / `{c as d}`), wildcard import of given
  instances (`import a.b.given`), and top-level definitions.
- Visibility ladder (`private`, `protected`, package-private via
  `private[pkg]`) maps to the Java `java_visibility_text` pattern.
- Companion objects, traits, and case classes are the Scala-specific deltas;
  they are encodable on top of Java's `class_declaration` /
  `interface_declaration` / `record_declaration` mapping with attribute
  decorations.

Family is `LanguageFamily::Scala` (already independent — confirmed in
`crates/squeezy-parse/src/backend.rs:34,82,96` and
`crates/squeezy-graph/src/backend.rs:24,54,68`). Java code is the **template**
to copy/adapt — it is **not** a parent backend. New file:
`crates/squeezy-parse/src/languages/scala.rs` (replaces current placeholder),
and `crates/squeezy-graph/src/languages/scala.rs` (new), wired into the graph
`mod.rs` and an entry on `SemanticGraph` analogous to `java_package_by_file`.

---

## 2. Grammar

Crate: **`tree-sitter-scala`** — reference grammar at
[`scalameta/tree-sitter-scala`](https://github.com/scalameta/tree-sitter-scala).
This is the only actively maintained Scala grammar and the one used by Neovim,
Helix, and Zed.

- Add to `crates/squeezy-parse/Cargo.toml`:
  ```toml
  tree-sitter-scala = "0.21"
  ```
- **VERIFY ON crates.io BEFORE MERGE.** As of last public release the crate
  was at `0.21.0` (Sep 2023) and supports Scala 3 syntax: indentation regions,
  `given`/`using`, `extension`, `enum`, `opaque type`, `inline`. If the live
  crates.io version has moved (e.g. 0.22+), pin to the highest version that
  still exposes a `LANGUAGE` constant (modern tree-sitter API). Older 0.20.x
  releases used the legacy `language()` fn and will not compile against the
  rest of squeezy's grammars.
- Wire into `crates/squeezy-parse/src/lib.rs`:
  - Add `scala_parser: Parser` to `LanguageParser`
    (`crates/squeezy-parse/src/lib.rs:228-241`).
  - Add `parser_with_scala_language()` helper next to
    `parser_with_java_language` (~line 705).
  - Add `LanguageKind::Scala => Ok(&mut self.scala_parser)` to
    `parser_for_language` (`crates/squeezy-parse/src/lib.rs:459-476`).
  - Add `LanguageKind::Scala => parser_with_scala_language()` to
    `parser_for_language_kind` (`crates/squeezy-parse/src/lib.rs:479-496`).
  - Add `fn scala_language() -> tree_sitter::Language { tree_sitter_scala::LANGUAGE.into() }`
    and the matching arm in `language_for_kind`
    (`crates/squeezy-parse/src/lib.rs:798-819`). Remove `LanguageKind::Scala`
    from the `None` fallthrough.

If `tree-sitter-scala` is not on crates.io with a modern API, fallback option:
vendor via git dependency pinned to a specific commit SHA, matching how other
projects consume it. Flag in PR description either way.

---

## 3. AST-node → fact mapping table

Scala 3 grammar node kinds (per `scalameta/tree-sitter-scala/grammar.js`)
mapped to squeezy facts in `crates/squeezy-core` (`SymbolKind`,
`ParsedSymbol`, `ParsedImport`, `ParsedCall`, `ParsedReference`,
`BodyHit`):

| Tree-sitter node | squeezy fact | `SymbolKind` | Confidence | Notes |
| --- | --- | --- | --- | --- |
| `compilation_unit` (root) | — | — | — | Traversal root. |
| `package_clause` | `ParsedImport` w/ alias `__scala_package__` | — | n/a | Mirrors Java's `__java_package__` sentinel (`languages/java.rs:30-34,266-290`). Multi-segment path joined with `.`. |
| `package_object` | `ParsedSymbol` + `ParsedImport` (package) | `Module` | High | `package object foo { ... }` emits a `Module` and registers `foo` as the file's package suffix. |
| `import_declaration` | `ParsedImport` (1..N) | n/a | n/a | See §4(m) for selector expansion. |
| `class_definition` | `ParsedSymbol` | `Class` | High | `extends`/`with` chain → `base:Name` attributes. |
| `case_class_definition` | `ParsedSymbol` + auto methods (see §4(d)) | `Struct` | High (decl) / Partial (auto-methods) | Use `Struct` (matches Java records → `SymbolKind::Struct` in `java_symbol_from_node`, `languages/java.rs:144-145`). Constructor params → `Field` children, mirroring `java_field_symbols_from_node`. |
| `object_definition` | `ParsedSymbol` | `Class` + attribute `scala:object` | High | Singleton. See §4(b)–(c). |
| `trait_definition` | `ParsedSymbol` | `Trait` | High | Same shape as Java `interface_declaration`. |
| `enum_definition` | `ParsedSymbol` | `Enum` | High | Scala 3 enum. |
| `enum_case` / `simple_enum_case` / `full_enum_case` | `ParsedSymbol` (child of enum) | `Variant` | High | One symbol per case; `case A, B, C` expands to N variants. |
| `type_definition` (abstract or alias) | `ParsedSymbol` | `TypeAlias` | High (top-level) / Partial (path-dependent — see §4(i)) |
| `opaque_modifier` on `type_definition` | as above + attribute `scala:opaque` | `TypeAlias` | High | |
| `function_definition` (`def`) | `ParsedSymbol` | `Method` if parent is class/trait/object; `Function` if top-level | High | `arity` from `parameters` field, matching `java_symbol_from_node`. |
| `given_definition` | `ParsedSymbol` | `Const` + attribute `scala:given` | Partial | See §4(f). Use `Const` (not `Field`) because given instances are top-level-eligible. |
| `extension_definition` | `ParsedSymbol` + child `function_definition` re-emit | `Function` + attribute `scala:extension`, `language_identity = receiver type text` | Partial | See §4(g). |
| `val_definition` / `val_declaration` | `ParsedSymbol` (1..N — pattern can bind multiple) | `Const` (if `parent` is class/object/trait) else `Const` at top level | High | Treat `val` as immutable `Const` so it matches Java `static final` shape. |
| `var_definition` / `var_declaration` | `ParsedSymbol` | `Field` (class member) / `Static` (top-level) | High | Mutable. |
| `class_parameter` (inside case class / class primary constructor) | `ParsedSymbol` | `Field` | High | One per parameter; emit attribute `type:T` per Java field convention. |
| `inline_modifier` on any def | as above + attribute `scala:inline` | unchanged | Partial (see §4(j)) | Body references demoted to Partial. |
| `call_expression` | `ParsedCall` (`ParsedCallKind::Method`) | n/a | `CandidateSet` | Mirrors `extract_java_method_invocation`. |
| `infix_expression` whose `operator` is an identifier | `ParsedCall` (`ParsedCallKind::Method`) | n/a | `Heuristic` | `a max b` desugars to `a.max(b)`. |
| `instance_expression` (`new Foo(...)`) | `ParsedCall` (`ParsedCallKind::Direct`) | n/a | `Heuristic` | Mirrors `extract_java_object_creation`. |
| `field_expression` | `ParsedReference` (`ReferenceKind::Field`) + `BodyHit` | n/a | n/a | Like Java `field_access`. |
| `stable_identifier` | `ParsedReference` (`ReferenceKind::Path`) | n/a | n/a | Dotted path; like Java `scoped_identifier`. |
| `type_identifier` | `ParsedReference` (`ReferenceKind::Type`) | n/a | n/a | |
| `generic_type` | recurse into `type_identifier` children | — | — | |
| `annotation` (`@Foo`) | `ParsedReference` (`ReferenceKind::Attribute`) + `BodyHit::Attribute` | n/a | n/a | Mirrors `extract_java_annotation_reference`. |
| literal nodes (`integer_literal`, `string`, `interpolated_string_expression`, …) | `BodyHit::Literal` | n/a | n/a | Use a `is_scala_literal(kind)` helper parallel to `is_java_literal`. |
| identifier in lvalue position of a binding | skipped (handled by binder rule above) | — | — | Avoid double-counting. |

Implementation skeleton: a `visit_scala_node` that mirrors `visit_java_node`
(`languages/java.rs:50-121`), with `scala_symbol_from_node` and
`scala_field_symbols_from_node` adapted from the Java equivalents. Reuse
`signature_text`, `span_from_node`, `node_text`, `symbol_id`,
`extract_body_hit`, `last_path_segment`, `named_child_count` from
`crates/squeezy-parse/src/languages/rust.rs` (already imported by
`java.rs:3`).

---

## 4. Language gotchas & heuristics

### (a) Indentation vs. braces

Scala 3 lets either `class Foo {  ... }` or `class Foo:\n  ...` form a body.
`tree-sitter-scala` resolves both to the same `*_definition` node with a
`body` field. No special handling. Open question (verify on real grammar):
whether the body-region span starts at `:` or at the first indented line. If
it starts at `:`, `body_span` will include the colon — acceptable, matches
Python `:`-prefixed bodies.

### (b) `object` declarations

`object Foo { ... }` is a JVM-level singleton class. Emit as
`SymbolKind::Class` with attribute `scala:object`. Confidence High. Members
emit as `Method` / `Const` children of the object, exactly like static
members on a Java class.

### (c) Companion objects

`object Foo` paired with `class Foo` in the same file. Detection: after
extracting all top-level symbols in a file, if a `Class`/`Struct`/`Trait`/
`Enum` named `N` and an `object` named `N` both have a `parent_id` that is
the file (or the same package object), attach attribute
`scala:companion-of:N` to the object and `scala:companion-object:N` to the
class. Members of the object are *not* re-parented onto the class (preserves
declaration locality); the graph resolver §3 below uses the attribute to
treat companion members as if statically imported on the class.

Reference: Kotlin already does the same thing in
`crates/squeezy-parse/src/languages/kotlin.rs` for `companion object`. Lift
that detection into a free helper if it's already factored out; otherwise
re-implement (Scala companions don't carry the `companion` keyword, so it's a
post-pass).

### (d) Case classes — auto-generated methods

`case class Point(x: Int, y: Int)` synthesizes `apply`, `unapply`, `copy`,
`equals`, `hashCode`, `toString`, plus an `unapply` on the companion. The
SemanticDB oracle (§9) **does emit these as occurrences** with
`SymbolOccurrence` rows pointing at the case-class span. To stay symmetric
with the oracle, squeezy must also emit them — but only as `Method` symbols
attached to the case-class symbol, each carrying
`Confidence::Partial` and attribute `scala:synthetic`.

If the oracle in a future tightening *does not* emit them (e.g. one of the
`OPERATOR` filters drops them), drop the emission. Decide which behavior the
oracle has during initial bring-up by running it against the §6 fixture and
diffing. Until then, default is to **emit + Partial**.

### (e) Traits

`trait Foo extends Bar with Baz` → `SymbolKind::Trait`. `extends` /
`with` clauses each contribute `base:<Name>` attributes (mirrors
Java pattern in `java_symbol_from_node`, `languages/java.rs:172-181`). Each
referenced name also emits a `ParsedReference` with
`ReferenceKind::Type`, used by inheritance-reference queries.

### (f) `given` / `using`

```scala
given intOrd: Ordering[Int] = ...
def sort[T](xs: List[T])(using ord: Ordering[T]): List[T] = ...
```

`given_definition` → `ParsedSymbol` with `SymbolKind::Const`, attribute
`scala:given`, optional attribute `scala:given-for:Ordering[Int]` derived
from the declared type. Confidence **Partial** (matters for §5).

`using` parameter clauses inside `function_definition`: the bound name (if
present) is treated like a regular parameter — emit as `Field` child with
attribute `scala:using`. Anonymous `using` parameters are skipped (no name).

**Call-site resolution of given/using is explicitly excluded** — the
extractor does not try to pick the matching `given` for a `using` clause
without a type checker. The oracle exclusion list in §9 mirrors this.

### (g) Extension methods

```scala
extension (s: String)
  def shout: String = s.toUpperCase
```

`extension_definition` wraps one or more inner `function_definition`s.
Emit each inner method as a top-level `Function` with:

- `language_identity = "String"` (the receiver type's textual form — keep
  the bracketed parts for generic receivers like `[T] (xs: List[T])`,
  shortened to `List[T]`).
- attribute `scala:extension`
- Confidence **High** if the receiver type is monomorphic and looks like a
  bare type identifier; **Partial** if the receiver involves a type
  parameter (extension on `[T] (xs: List[T])` resolves at the call site
  through type inference we don't have).

### (h) Implicit conversions (Scala 2 holdover)

`implicit def fooToBar(x: Foo): Bar = ...` — emit as a regular
`Function`/`Method` with attribute `scala:implicit-conversion`. **Their
injection at call sites is excluded.** A call like `someFoo.barMethod`
where `barMethod` exists on `Bar` is not resolved by squeezy. This is an
FN bucket; the oracle exclusion list in §9 keeps it from being scored.

### (i) Path-dependent types

```scala
class A { type B }
val a = new A
val x: a.B = ...
```

Declaration: emit `B` as a `TypeAlias` child of `A` (High). Reference:
`a.B` produces a `ParsedReference` (`ReferenceKind::Type`) on the text
`a.B`. The graph resolver does **not** try to resolve `a.B` to `A#B` —
that needs a type checker. References to path-dependent types are
**Partial** confidence implicitly (no resolution, so the reference exists
but no edge is created).

### (j) Macros / inline

`inline def m(...) = ${ ... }` (Scala 3 macros) and any `inline def` body
expansion is excluded. The declaration emits normally; the body is walked
to extract `BodyHit`s but every `ParsedCall` and `ParsedReference`
emitted inside an `inline_modifier`-tagged def carries
`Confidence::Heuristic` (downgrade one step from the default).

### (k) Top-level declarations

Scala 3 allows `def`/`val`/`given` at the top level of a file outside any
object:

```scala
package foo
def topLevelFn(): Int = 42
val topLevelVal = 7
```

Emit with `parent_id = None` (file-level) and use the file's package as
the symbolic parent for cross-file resolution. Set `SymbolKind` to
`Function` (for `def`), `Const` (for `val`), `Static` (for `var`),
`Const` + `scala:given` (for `given`). The graph resolver treats
top-level Scala defs analogously to Java *static imports*: any file in
the same package can call `topLevelFn()` unqualified.

### (l) Anonymous classes / lambdas

`new Foo { def bar = ... }` and `(x: Int) => x + 1` are excluded. The
synthesized `Foo$anon`/`$anonfun` symbols don't appear in tree-sitter
output, so this is mostly automatic; just don't try to invent them.

### (m) Imports

Scala 3 import selectors:

| Source | `ParsedImport` shape |
| --- | --- |
| `import a.b.c` | `path: "a.b.c"`, `kind: Named`, `imported_name: Some("c")`, alias `None`. |
| `import a.b.*` | `path: "a.b.*"`, `is_glob: true`, `kind: Wildcard`, `imported_name: None`. |
| `import a.b.{c, d}` | Two `ParsedImport`s: `a.b.c` + `a.b.d`, both Named. |
| `import a.b.{c as e}` (Scala 3) or `import a.b.{c => e}` (Scala 2 syntax still accepted) | `path: "a.b.c"`, `kind: Named`, `imported_name: Some("c")`, `alias: Some("e")`. |
| `import a.b.given` | `path: "a.b"`, `is_glob: true`, `kind: Wildcard`, attribute `scala:import-given`, `imported_name: None`. Treated like a wildcard for the given namespace. |
| `import a.b.{given Ordering[Int]}` | `path: "a.b"`, `is_glob: false`, `kind: Named`, `imported_name: Some("Ordering")`, attribute `scala:import-given`. |
| `import a.b.{c, given}` | The `c` selector becomes a Named import; the `given` selector becomes a `scala:import-given` Wildcard as above. |

Implementation: walk the `import_declaration` node's selector list rather
than text-parsing the raw source (the Java extractor uses
`raw.strip_prefix("import")` because Java imports are flat — Scala's
selectors are structured AST nodes and need real traversal). Attributes go
on the new `ParsedImport::attributes` field — **if no such field exists
yet, encode in `alias` with sentinel prefix** like the Java code does for
`__java_package__`. Prefer adding the field on a follow-up if it lands
cleanly in one shape; do not block this PR on a core-type addition.

---

## 5. Per-symbol confidence rules

Default: `Confidence::ExactSyntax` for any declaration with a name and span
that comes straight from a `*_definition` AST node.

Downgrades:

| Case | Confidence |
| --- | --- |
| Static class / object / trait / enum / type-alias declarations | High (`ExactSyntax`) |
| Method declarations (incl. inside companion object) | High |
| Top-level `def` / `val` / `var` | High |
| Case-class auto-generated methods (`apply`/`unapply`/`copy`/...) | Partial |
| `given_definition` | Partial |
| Extension method whose receiver involves a type parameter | Partial |
| Path-dependent type *reference* (`a.B`) | Partial (via `ParsedReference` with no resolution; no edge formed) |
| Any symbol, call, or reference inside an `inline def` body | Heuristic |
| References extracted from macro splice `${ ... }` blocks | Heuristic |
| Reference to a name that resolves only via an `import a.b.given` wildcard | Partial |

The `ParsedSymbol::provenance` field carries
`Provenance::new("tree-sitter-scala", "<node-kind> declaration")` for every
emission, matching the Java pattern.

---

## 6. Fixture sketch

Layout under `benchmarks/fixtures/scala/semantic-cases/` (mirrors
`benchmarks/fixtures/java/semantic-cases/`):

```
benchmarks/fixtures/scala/semantic-cases/
  build.sbt                            # SBT marker for source roots
  pom.xml                              # also present so Maven project-facts apply
  src/main/scala/example/app/Runner.scala
  src/main/scala/example/util/Names.scala
  src/main/scala/example/services/Greeter.scala
  src/main/scala/example/services/FriendlyGreeter.scala
  src/main/scala/example/ext/StringOps.scala
  src/main/scala/example/opaque/Money.scala
  src/generated/scala/example/generated/GeneratedGreeter.scala
  vendor/com/example/Ignored.scala
```

Coverage:

1. **`Runner.scala`** — `package example.app`, imports for both
   `example.util.Names.*` and `example.services.{Greeter, FriendlyGreeter}`.
   Top-level `def buildDefault(): Runner = new Runner(...)`. A `class Runner`
   that holds a `val greeter: Greeter` field and calls `greeter.greet(...)`
   inside a `def run(): Unit`. Covers cross-file references, method
   invocations on a field receiver, top-level def, named imports.
2. **`Greeter.scala`** — `sealed trait Greeter { def greet(name: String): String }`
   plus `object Greeter` (companion) with a `def default: Greeter = ...`
   factory. Covers trait + companion-object pair (§4(c)) and inheritance
   reference target.
3. **`FriendlyGreeter.scala`** — `case class FriendlyGreeter(prefix: String)
   extends Greeter`. Covers case-class auto methods (§4(d)) and trait
   inheritance.
4. **`Names.scala`** — `package example.util` with `enum Names { case Alice,
   Bob, Carol }` plus a `def defaultName: Names = Names.Alice`. Covers Scala 3
   `enum`, enum-case resolution.
5. **`StringOps.scala`** — `extension (s: String) def shout: String =
   s.toUpperCase` plus `extension [T] (xs: List[T]) def secondOpt:
   Option[T] = xs.drop(1).headOption`. Used cross-file from `Runner.scala`
   (`"hello".shout`). Covers extension on monomorphic (High) and generic
   (Partial) receivers.
6. **`Money.scala`** — `opaque type Money = BigDecimal` plus `object Money`
   with `def apply(x: BigDecimal): Money = x`. A `given Ordering[Money] = ...`.
   Covers opaque type, given declaration.
7. **`generated/.../GeneratedGreeter.scala`** — synthetic ScalaPB-style
   `final case class GeneratedGreeter(...)` with package
   `example.generated`. Asserts that `src/generated/scala` shows up as a
   `generated_exclusion` source root in project facts.
8. **`vendor/com/example/Ignored.scala`** — file under `vendor/` to assert
   the same exclusion path the Java fixture uses (file is excluded from
   the symbol scan).
9. **`build.sbt`** — minimal:
   ```scala
   scalaVersion := "3.5.0"
   libraryDependencies ++= Seq(
     "org.scalatest" %% "scalatest" % "3.2.18" % Test,
     "com.lihaoyi" %% "utest" % "0.8.3" % Test
   )
   ```
   Covers SBT-style dependency extraction (needs a new
   `sbt_dependency_facts` parser alongside `maven_dependency_facts` /
   `gradle_dependency_facts`).
10. **`pom.xml`** — duplicated from the Java fixture (slight rename) to
    keep `java_dependency_facts` working unchanged when a Scala project
    also uses Maven (Scala+Maven is common).

---

## 7. Real-repo corpus

**Primary:** [`lihaoyi/utest`](https://github.com/com-lihaoyi/utest)
— note the canonical repo is `com-lihaoyi/utest`, not `lihaoyi/utest`.

- Suggested tag: `0.8.5` (latest stable as of late 2024 — verify and bump
  before merge).
- Smoke subset (~80 files): `utest/src/utest/` (the core source root). Use
  whichever module directory holds the Scala 3 cross-build outputs at the
  pinned tag.
- Why utest: small (under 100 source files), idiomatic modern Scala
  (cross-builds 2.13 + 3.x; we want the Scala 3 sources), no SBT-magic
  source generation, no large macro-heavy framework. Lots of `given` /
  extension / opaque-type usage, which exercises the gotchas in §4.

**Alternative:** [`softwaremill/sttp`](https://github.com/softwaremill/sttp)
— pin a 4.x tag. Larger but still Scala 3 friendly. Use only if utest
proves too small to surface signal during accuracy tuning.

corpus.json entry (place after the `java-smoke` block,
`benchmarks/corpus.json:32-40`):

```json
{
  "name": "scala-smoke",
  "family": "scala",
  "language": "scala",
  "tier": "smoke",
  "fixture": "benchmarks/fixtures/scala/semantic-cases",
  "spec": "benchmarks/specs/scala-smoke-queries.json",
  "report": "scala/scala-smoke.json",
  "ra_lsp_probes": 0
},
{
  "name": "utest",
  "family": "scala",
  "language": "scala",
  "tier": "full",
  "fixture": "target/benchmark-repos/utest/utest/src",
  "spec": "benchmarks/specs/empty-queries.json",
  "report": "scala/utest.json",
  "ra_lsp_probes": 0,
  "no_speed_gate": true,
  "repo": {
    "url": "https://github.com/com-lihaoyi/utest",
    "rev": "<pin to 0.8.5 commit SHA>",
    "checkout": "target/benchmark-repos/utest"
  }
}
```

Set `ra_lsp_probes: 25` only on the smoke entry, and only after §9's
SemanticDB oracle is wired to actually answer definition/reference probes.
Initial PR: leave at 0.

---

## 8. Smoke query spec

`benchmarks/specs/scala-smoke-queries.json`:

```json
{
  "queries": [
    {
      "id": "scala-hierarchy",
      "kind": "hierarchy_contains",
      "expected_contains": [
        "Class:Runner",
        "Class:Greeter",
        "Trait:Greeter",
        "Struct:FriendlyGreeter",
        "Enum:Names",
        "Variant:Alice",
        "Variant:Bob",
        "Variant:Carol",
        "Method:run",
        "Method:greet",
        "Method:default",
        "Function:buildDefault",
        "Function:shout",
        "Function:secondOpt",
        "TypeAlias:Money",
        "Class:Money",
        "Const:intOrd"
      ]
    },
    {
      "id": "scala-trait-signature",
      "kind": "signature_search",
      "text": "trait Greeter",
      "symbol_kind": "Trait",
      "expected_contains": [
        "Trait:Greeter"
      ]
    },
    {
      "id": "scala-trait-abstract-member",
      "kind": "signature_search",
      "text": "def greet(name: String): String",
      "symbol_kind": "Method",
      "expected_contains": [
        "Method:greet"
      ]
    },
    {
      "id": "scala-inheritance-reference",
      "kind": "references_to_symbol",
      "to": "Greeter",
      "expected_contains": [
        "Greeter"
      ]
    },
    {
      "id": "scala-extension-cross-file-call",
      "kind": "call_chain",
      "from": "run",
      "to": "shout",
      "expected_contains": [
        "run -> shout"
      ]
    },
    {
      "id": "scala-companion-factory-call",
      "kind": "call_chain",
      "from": "buildDefault",
      "to": "default",
      "expected_contains": [
        "buildDefault -> default"
      ]
    },
    {
      "id": "scala-enum-case-resolution",
      "kind": "reference_search",
      "text": "Alice",
      "expected_contains": [
        "Alice"
      ]
    },
    {
      "id": "scala-given-emission",
      "kind": "signature_search",
      "text": "given Ordering[Money]",
      "symbol_kind": "Const",
      "expected_contains": [
        "Const:"
      ]
    },
    {
      "id": "scala-project-facts",
      "kind": "scala_project_facts",
      "expected_contains": [
        "sbt:source_root:main:src/main/scala",
        "sbt:dependency:Test:org.scalatest:scalatest:3.2.18",
        "maven:source_root:main:src/main/java"
      ]
    },
    {
      "id": "scala-fallback-quality",
      "kind": "fallback_quality",
      "expected_contains": [
        "generated",
        "vendor"
      ]
    }
  ]
}
```

The `scala_project_facts` query kind is new — add to the query dispatcher
parallel to `java_project_facts`. If that's heavier than this PR can
absorb, drop the query for the first cut and add a TODO; the §6 fixture
still works without it.

The `scala-given-emission` query uses `Const:` as an open-ended contains
check — we assert the kind is emitted; the actual name (`intOrd`,
`MoneyOrdering`, …) varies by fixture authoring.

---

## 9. Oracle plan

### Tool: Scalameta + SemanticDB

SemanticDB is the Scala equivalent of an LSIF dump — purpose-built for
"language-server-friendly serialized AST" consumption. It is the format
backing Metals (the Scala LSP). It carries:

- `SymbolOccurrence` rows: `(range, symbol, role)` where role ∈
  `{DEFINITION, REFERENCE}`. Definition probes and reference probes are
  both directly supported.
- `SymbolInformation` rows: `(symbol, kind, signature, language, ...)`
  for every declared symbol.
- Stored as protobuf in `META-INF/semanticdb/<path>.scala.semanticdb`
  alongside compiled class files.

The compiler flag is `-Xsemanticdb` (Scala 3) /
`-Yrangepos -Ysemanticdb` (Scala 2). Scala 3.5+ uses
`scalac -Xsemanticdb -semanticdb-target:<dir> <sources>`.

**Rejected alternative:** `scalac -Yshow-trees-stringified` / `-Vprint`.
The format is the compiler's internal Trees printer, which mutates across
minor Scala versions, has no documented schema, and bakes in desugarings
(every `val x = 1` becomes `val x: Int = 1`, every for-comprehension
becomes `map`/`flatMap`/`withFilter`). Too unstable; SemanticDB is
explicitly versioned (schema lives at
`scalameta/scalameta/semanticdb/semanticdb/semanticdb3.proto`).

### Helper layout

`benchmarks/oracle-helpers/scala-oracle/`:

```
benchmarks/oracle-helpers/scala-oracle/
  README.md
  scala-oracle.sc          # scala-cli script (preferred)
  scala-oracle.scala       # equivalent .scala source
```

`scala-oracle.sc` (scala-cli script — single-file, no SBT):

```scala
//> using scala 3.5.0
//> using dep org.scalameta:semanticdb-shared_3:4.9.9
//> using dep org.scalameta:semanticdb-scalac_2.13:4.9.9 // for cross-version reads
import scala.meta.internal.semanticdb._
import java.nio.file._, scala.jdk.CollectionConverters._

@main def run(sdbDir: String, rootDir: String): Unit = {
  val root = Paths.get(rootDir).toAbsolutePath.normalize
  val rows = scala.collection.mutable.ArrayBuffer.empty[(String, String, String)]
  Files.walk(Paths.get(sdbDir)).iterator.asScala
    .filter(_.toString.endsWith(".semanticdb"))
    .foreach { p =>
      val doc = TextDocuments.parseFrom(Files.readAllBytes(p)).documents.head
      val relSource = root.relativize(Paths.get(doc.uri)).toString.replace('\\', '/')
      doc.symbols.foreach { si =>
        val kind = si.kind match {
          case SymbolInformation.Kind.CLASS  => "Class"
          case SymbolInformation.Kind.TRAIT  => "Trait"
          case SymbolInformation.Kind.OBJECT => "Class" // squeezy treats objects as Class
          case SymbolInformation.Kind.METHOD => "Method"
          case SymbolInformation.Kind.MACRO  => "Method"
          case SymbolInformation.Kind.TYPE   => "TypeAlias"
          case SymbolInformation.Kind.FIELD  => "Field"
          case SymbolInformation.Kind.LOCAL  => "_skip"
          case _                              => "_skip"
        }
        if (kind != "_skip") rows += ((relSource, kind, si.displayName))
      }
    }
  print(rows.distinct.map { case (f, k, n) =>
    s"""["$f","$k","${n.replace("\\", "\\\\").replace("\"", "\\\"")}"]"""
  }.mkString("""{"rows":[""", ",", "]}"))
}
```

Runner (in Rust under
`benchmarks/squeezy-graph-bench/src/oracles/scala_semanticdb.rs`):

1. `temp = temp_dir("squeezy-scala-oracle")?` for the `.semanticdb` output.
2. `scalac -Xsemanticdb -semanticdb-target:<temp> -d <temp>/classes
   <all .scala files under root>`.
3. `scala-cli run scala-oracle.sc -- <temp> <root>`.
4. Parse the JSON, drop rows whose path is in `exclusions`, normalize
   names, fold into `SymbolScan` (mirroring
   `collect_java_compiler_tree_symbol_scan`,
   `benchmarks/squeezy-graph-bench/src/oracles/javac.rs:124-168`).

If `scala-cli` is missing but `protoc` is, **fallback path**: implement
the `.semanticdb` reader directly in Rust using the
[`prost`](https://crates.io/crates/prost) crate plus a vendored
`semanticdb3.proto` (the file is ~400 lines, public domain). This drops
the JVM dependency for the parsing half but still requires `scalac` for
producing the `.semanticdb` files. Worth doing in a follow-up to remove
the scala-cli install on CI runners.

### Install in CI

In `.github/actions/setup-bench/action.yml` (the action invoked by
`.github/workflows/benchmark-lang.yml:71-74`), add a `scala` branch:

```yaml
- name: Install Coursier and Scala 3
  if: inputs.language == 'scala'
  shell: bash
  run: |
    curl -fLo cs https://github.com/coursier/launchers/raw/master/cs-x86_64-pc-linux.gz \
      | gunzip > cs
    chmod +x cs
    sudo mv cs /usr/local/bin/cs
    cs setup --yes
    cs install --quiet scala3-compiler scala-cli
    echo "$HOME/.local/share/coursier/bin" >> "$GITHUB_PATH"
- name: Setup JDK 17
  if: inputs.language == 'scala'
  uses: actions/setup-java@v4
  with:
    distribution: temurin
    java-version: '17'
```

Pin `cs` launcher commit SHA at merge time (Coursier publishes signed
launchers but doesn't tag them on GitHub).

### Scan strategy

Per-source-root, single `scalac` invocation:

```
scalac -Xsemanticdb \
       -semanticdb-target:/tmp/sdb \
       -d /tmp/classes \
       $(find <root> -name '*.scala' -not -path '*/vendor/*')
```

`scalac` is slow (3–10s startup; ~50 files/s after warm-up). Cache the
`.semanticdb` output keyed on the (sorted file paths × file content
hashes) tuple — drop into `target/oracle-cache/scala/<hash>/` and reuse
across benchmark runs. This matches the pattern used by the Roslyn
oracle (`benchmarks/oracle/csharp/CsharpOracle.csproj` pre-build cache,
`benchmark-lang.yml:76-80`).

### Exclusion list (oracle limitations)

```rust
pub(crate) fn scala_oracle_limitations() -> Vec<String> {
    vec![
        "Scala oracle uses SemanticDB declarations; implicit-conversion injection at call sites, `given`/`using` resolution at call sites, and macro-expanded synthetic members are excluded from the symbol comparison.".to_string(),
        "Path-dependent type references (`a.B`) are emitted as references with no resolution edge; they are excluded from navigation accuracy.".to_string(),
        "Anonymous classes and lambda bodies are not compared; SemanticDB emits `<anon>` symbols that the tree-sitter extractor omits.".to_string(),
        "Local `val`/`var` (LOCAL kind in SemanticDB) are excluded — squeezy does not emit locals as symbols.".to_string(),
        "If `scalac` or `scala-cli` is unavailable, the oracle is skipped while fixture query gates still run.".to_string(),
    ]
}
```

### Definition / reference probes

SemanticDB carries `role`, so both `goto definition` and
`find references` map cleanly. After the symbol-scan parity is in,
enable `ra_lsp_probes: 25` on `scala-smoke` (rename the field name later
if its `ra_` prefix bothers — it's currently a generic "probe budget").
The probe driver re-uses `collect_query_oracle_accuracy`
(`benchmarks/squeezy-graph-bench/src/oracles/javac.rs:82-109`) shape.

### Scan-only fallback

If neither `scalac` nor `cs` is on `$PATH`, degrade to
`collect_squeezy_symbol_scan` (the same "no oracle" code path Java uses
when `java` is missing, `oracles/javac.rs:34-45`) and stamp
`status: "skipped: scalac not found"`. Fixture query gates continue to
run unaffected.

---

## 10. Gate thresholds for first PR

Per `benchmarks/squeezy-graph-bench/src/gates.rs:5-58`, gates are
all-or-nothing on the configured oracle. For the Scala oracle:

- **Precision: ≥ 0.90.** Most squeezy emissions correspond to a real
  SemanticDB declaration; the remaining ≤10% is the case-class synthetic
  set (§4(d)) plus any companion-object members we double-attach via the
  graph resolver.
- **Recall: ≥ 0.75.** SemanticDB emits a lot of stuff squeezy doesn't —
  primary-constructor desugarings, `apply`/`unapply` companion shims, the
  `package $package$` synthetic, locals (already excluded above but
  worth restating), and anonymous-class bodies. 0.75 leaves headroom for
  the implicit-conversion and given-resolution gaps that aren't going to
  close in a static extractor.

Codify in a new `scala_oracle` check inside `enforce_gates`
(`benchmarks/squeezy-graph-bench/src/gates.rs:39-47`), mirroring the Go
oracle's hard equality but using ratio thresholds instead:

```rust
if !no_speed_gate
    && let Some(scala) = &report.scala_oracle
    && (scala.symbols.precision < 0.90 || scala.symbols.recall < 0.75)
{
    return Err(SqueezyError::Graph(format!(
        "Scala oracle accuracy below gate: precision={:.3} recall={:.3}",
        scala.symbols.precision, scala.symbols.recall
    )));
}
```

Keep behind `no_speed_gate: true` initially on the `utest` full-tier
entry; let the smoke fixture enforce.

---

## 11. Speed parity target

Per-file `parse_record` + `extract_scala` time within **2× of Java's**
(`tree-sitter-java` + `extract_java`) on the §6 fixture. Justification:

- `tree-sitter-scala` is heavier than `tree-sitter-java` because the
  grammar has more productions (indentation handling, `given`, extension,
  `inline`, `enum`, opaque types, plus full Scala 2 backward compat).
  Real-world: ~1.5–1.8× parser cost on typical inputs.
- Extractor walk is the same shape and node count as Java's;
  per-file allocator pressure should match.

Add a benchmark assertion in
`benchmarks/squeezy-graph-bench/src/report.rs` (or wherever
`per_file_parse_ms` lives — track down during impl) gating
`scala_per_file_parse_ms <= java_per_file_parse_ms * 2.0`. If this is
not yet a tracked field, drop the assertion and add a note in the PR
that the speed gate is opt-in until the field exists. Don't synthesize
a new metric for one language.

The mixed-workload speed gate (the global "faster than validation"
check in `gates.rs:23-28`) is set to `no_speed_gate: true` on the
`utest` full-tier entry — SemanticDB compilation will trivially be
slower than squeezy's tree-sitter walk, but the gate compares squeezy
total to validation total. Keep the smoke entry without
`no_speed_gate` so the comparison still runs against a fixture-scale
SemanticDB invocation.

---

## 12. CI matrix entry

Add to `.github/workflows/benchmark-lang.yml`. Two edits:

(a) Add `scala` to the `workflow_dispatch` language `options`
(`benchmark-lang.yml:33-41`):

```yaml
options:
  - rust
  - python
  - java
  - scala
  - go
  - c-family
  - csharp
  - js-ts
```

(b) Extend the runner / timeout matrix
(`benchmark-lang.yml:63-64`):

```yaml
runs-on: ${{ inputs.language == 'rust' && 'macos-latest' || 'ubuntu-latest' }}
timeout-minutes: ${{
  (inputs.language == 'c-family' || inputs.language == 'csharp') && 90 ||
  inputs.language == 'go' && 60 ||
  inputs.language == 'scala' && 90 ||
  120 }}
```

(c) Caller workflow (typically
`.github/workflows/benchmark-rust.yml` and siblings — add
`benchmark-scala.yml` paralleling them):

```yaml
name: Scala benchmark
on:
  pull_request:
    paths:
      - 'crates/squeezy-parse/src/languages/scala.rs'
      - 'crates/squeezy-graph/src/languages/scala.rs'
      - 'benchmarks/fixtures/scala/**'
      - 'benchmarks/specs/scala-smoke-queries.json'
      - '.github/workflows/benchmark-scala.yml'
      - '.github/workflows/benchmark-lang.yml'
  workflow_dispatch:
jobs:
  smoke:
    uses: ./.github/workflows/benchmark-lang.yml
    with:
      language: scala
      tier: smoke
      artifact-suffix: scala
      summary-file: __scala_summary.md
    continue-on-error: true
```

`continue-on-error: true` is critical for the **first** PR — gives the
landing branch a green merge even if SemanticDB install or `scalac`
download flakes on a runner. Flip to `false` in a follow-up once a few
green runs are observed.

The setup action change in §9 (Coursier + JDK 17 install gated on
`inputs.language == 'scala'`) goes in `.github/actions/setup-bench/action.yml`,
not the workflow file.

---

## Implementation checklist (for the follow-up PR)

1. `crates/squeezy-parse/Cargo.toml`: add `tree-sitter-scala = "0.21"`
   (verify version on crates.io first).
2. `crates/squeezy-parse/src/lib.rs`: wire scala parser per §2.
3. `crates/squeezy-parse/src/languages/scala.rs`: replace placeholder with
   the §3+§4 extractor (port from `java.rs`).
4. `crates/squeezy-graph/src/languages/scala.rs`: new file; mirror
   `java.rs` resolver — `scala_package_for_file`, `scala_import_matches_symbol`
   (with selector-aware matching from §4(m)), `scala_companion_for_class`,
   and a top-level-def candidate filter.
5. `crates/squeezy-graph/src/languages/mod.rs`: register the new module.
6. `crates/squeezy-graph/src/lib.rs` (or wherever
   `java_package_by_file` lives on `SemanticGraph`): add
   `scala_package_by_file`.
7. `benchmarks/fixtures/scala/semantic-cases/`: create per §6.
8. `benchmarks/specs/scala-smoke-queries.json`: create per §8.
9. `benchmarks/corpus.json`: add `scala-smoke` and `utest` entries per
   §7.
10. `benchmarks/squeezy-graph-bench/src/oracles/scala_semanticdb.rs`:
    new oracle per §9.
11. `benchmarks/squeezy-graph-bench/src/report.rs`: add
    `scala_oracle: Option<ScalaOracleReport>` plus the report type.
12. `benchmarks/squeezy-graph-bench/src/gates.rs`: add scala gate per
    §10.
13. `benchmarks/oracle-helpers/scala-oracle/scala-oracle.sc`: new helper
    per §9.
14. `.github/actions/setup-bench/action.yml`: add Coursier/JDK steps.
15. `.github/workflows/benchmark-lang.yml`: add `scala` option + timeout.
16. `.github/workflows/benchmark-scala.yml`: new caller workflow.

---

## Open questions to resolve during impl

- **`tree-sitter-scala` exact version on crates.io.** Verify and pin
  before opening the PR. If the only modern-API release is via git, vendor.
- **Whether `ParsedImport` should grow an `attributes` field** for
  `scala:import-given` and friends, or whether to encode in `alias` with
  a sentinel. Defer until §4(m) implementation surfaces the awkwardness.
- **Whether case-class auto-methods land on Scala side or not.** Verify
  empirically against SemanticDB on the §6 fixture during bring-up.
- **`scala_project_facts` query kind.** Drop on the first PR if it
  forces a query-engine change; come back in a follow-up.
- **`utest` revision SHA.** Pin at PR time; don't reference a branch.
