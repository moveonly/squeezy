# Kotlin language-implementation spec

Planning artifact for the `langs/kotlin` follow-up commit. Scaffolding
(`LanguageKind::Kotlin`, `LanguageFamily::Kotlin`, `.kt`/`.kts` mapping,
`KotlinBackend`, `KotlinGraphExt`, and `extract_kotlin` returning
`ParsedFile::unsupported(...)`) has already landed; this document defines the
extractor, graph wiring, fixtures, oracle, and CI gates the per-language PR
needs to deliver.

Scope of this spec: a single PR that fills in `extract_kotlin`, adds
`tree-sitter-kotlin` as a workspace dep, adds a smoke fixture, smoke query
spec, a JetBrains compiler-embeddable oracle, and a `kotlin` family entry in
the benchmark workflows. It does not aim at full type-aware navigation; that
is a phase-2 enhancement once the symbol-set gates hold.

---

## 1. Template choice

Primary template: `crates/squeezy-parse/src/languages/java.rs` and
`crates/squeezy-graph/src/languages/java.rs`.

Why Java: Kotlin compiles to the JVM, shares the file-as-compilation-unit
model (with `package` headers and `import` statements), and resolves
type/method binding through a classpath. Classes, interfaces, enums, and
methods map onto the same `SymbolKind` variants squeezy already emits for
Java. Cross-file resolution uses the same package + import + receiver-type
chain logic that `java_import_matches_symbol`,
`java_symbol_owner_path`, `java_static_imported_method`, and
`java_receiver_field_method` implement today.

What is *not* shared: the orchestration plan keeps `LanguageFamily::Java` and
`LanguageFamily::Kotlin` independent. The Kotlin extractor and graph helpers
are a copy-and-modify of the Java pair (`extract_kotlin`,
`visit_kotlin_node`, `kotlin_symbol_from_node`, `kotlin_import_matches_symbol`,
etc.) — *no* shared `pub(crate)` surface across families, no shared
constants. This matches the pattern Ruby/PHP/Scala scaffolding sets up:
identical code shape but no cross-family coupling, so a future Java refactor
cannot silently break Kotlin.

What is *added*: companion-object flattening, top-level
function/property emission keyed on the file package, extension-function
receiver capture into `language_identity`, type-alias handling, and the
`suspend` attribute flag. Each is local to the Kotlin extractor.

---

## 2. Grammar

**Recommended dependency** (Cargo workspace `crates/squeezy-parse/Cargo.toml`):

```toml
tree-sitter-kotlin = "0.3"
```

**Verify before merge**: as of writing, the most-maintained Kotlin grammar
package on crates.io has historically been `tree-sitter-kotlin` published off
the `fwcd/tree-sitter-kotlin` upstream, with the 0.3.x line being current.
The implementing PR must:

1. `cargo search tree-sitter-kotlin` and confirm a 0.3.x (or newer 0.x)
   release exists, then pin to an exact patch version that vendors a
   precompiled grammar with `LANGUAGE: LanguageFn`.
2. If no stable crate is available or the published crate is unmaintained,
   fall back to a git pin against the upstream fork:

   ```toml
   tree-sitter-kotlin = { git = "https://github.com/fwcd/tree-sitter-kotlin", rev = "<sha-pin>" }
   ```

   and document the rev in `Cargo.toml` with a one-line comment naming the
   commit date.
3. Confirm the crate exposes a `LANGUAGE: LanguageFn` constant compatible
   with `tree-sitter::Parser::set_language` the same way `tree_sitter_java`
   does — the registration site at
   `crates/squeezy-parse/src/lib.rs:752-810` follows that signature.
4. Run `cargo deny` and confirm the license is MIT or Apache-2.0
   (in line with the existing tree-sitter family).

Wiring once the dep is in place:

- Add `kotlin_parser: Parser` to `LanguageParser` struct
  (`crates/squeezy-parse/src/lib.rs:228-246`).
- Construct via `parser_with_kotlin_language()?` in `LanguageParser::new`
  (`:260-287`).
- Add `LanguageKind::Kotlin => Ok(&mut self.kotlin_parser)` arm to
  `parser_for_language` (`:459-477`).
- Add `LanguageKind::Kotlin => parser_with_kotlin_language()` arm to
  `parser_for_language_kind` (`:479+`).
- Add a `kotlin_language()` helper alongside `java_language()`
  (`:752-810`) and a `LanguageKind::Kotlin => Some(kotlin_language())`
  arm in `language_for_kind`.

---

## 3. AST-node → fact mapping

Node kinds below are from the `fwcd/tree-sitter-kotlin` grammar. The
implementing PR should add a unit test that emits the node-kind list of a
representative `.kt` file under `crates/squeezy-parse/src/languages/kotlin_tests.rs`
to catch grammar churn.

| tree-sitter node kind          | Fact emitted                                              | `SymbolKind` / kind   | Notes                                                                                                 |
| ------------------------------ | --------------------------------------------------------- | --------------------- | ----------------------------------------------------------------------------------------------------- |
| `package_header`               | `ParsedImport` with `alias = Some("__kotlin_package__")`  | n/a                   | Same package-as-import trick Java uses (`__java_package__`) so `kotlin_package_for_file` can find it. |
| `import_header` (no glob, no `as`) | `ParsedImport`, `kind = ImportKind::Named`             | n/a                   | `imported_name = Some(last_path_segment(&path))`.                                                     |
| `import_header` with `*`       | `ParsedImport`, `kind = ImportKind::Wildcard`, `is_glob = true` | n/a             | `imported_name = None`.                                                                               |
| `import_header` with `as <alias>` | `ParsedImport`, `kind = ImportKind::Named`, `alias = Some(alias)` | n/a       | Strip the alias from `path`, use it as the binding name on resolution.                                |
| `class_declaration`            | `ParsedSymbol`                                            | `Class`               | `arity = None`. Extend `attributes` with `base:<parent>` for each `delegation_specifier`.             |
| `object_declaration`           | `ParsedSymbol`                                            | `Class`               | Singleton. `Confidence::ExactSyntax`. Attribute `kotlin:object`.                                      |
| `companion_object`             | `ParsedSymbol`                                            | `Class`               | Hosted under the enclosing class. Children emitted as if static members (see gotcha 4b).              |
| `function_declaration`         | `ParsedSymbol`                                            | `Method` if parent is class-like else `Function` | `arity = parameter count`. Add `kotlin:suspend`/`kotlin:inline` attributes when applicable.   |
| `property_declaration`         | `ParsedSymbol`                                            | `Field` if parent is class-like else `Const`/`Static` | Treat `val` and top-level read-only as `Const`; class-level as `Field`.              |
| `interface_declaration`        | `ParsedSymbol`                                            | `Trait`               | Same as Java interfaces.                                                                              |
| `enum_class_declaration`       | `ParsedSymbol`                                            | `Enum`                | Children: `enum_entry` → `Variant`.                                                                   |
| `enum_entry`                   | `ParsedSymbol`                                            | `Variant`             | Parent is the enum.                                                                                   |
| `secondary_constructor`        | `ParsedSymbol`                                            | `Method`              | `name = enclosing class name` (mirrors Java `<init>` handling in `JavaOracle.visitMethod`).            |
| `type_alias`                   | `ParsedSymbol`                                            | `TypeAlias`           | `language_identity = Some(<alias target>)`.                                                            |
| `call_expression`              | `ParsedCall`, `kind = Method` when navigation-prefixed, else `Direct` | n/a       | `arity` from `value_arguments` named child count.                                                     |
| `navigation_expression`        | `ParsedReference` `ReferenceKind::Field` (terminal) and `Path` (multi-segment) | n/a | Same shape as Java `field_access`/`scoped_identifier`.                                                |
| `type_identifier`              | `ParsedReference` `ReferenceKind::Type`                   | n/a                   | Strip keyword filter (`is_kotlin_keyword`).                                                            |
| `simple_identifier`            | *(suppressed)* — too noisy                                | n/a                   | Same as Java `"identifier"` arm.                                                                       |
| `annotation`                   | `ParsedReference` `ReferenceKind::Attribute` + `BodyHit`  | n/a                   | Mirror `extract_java_annotation_reference`.                                                            |
| literals (`integer_literal`, `string_literal`, `boolean_literal`, `character_literal`) | `BodyHit { kind: Literal }` | n/a | Wrap `is_kotlin_literal(kind)` like `is_java_literal`. |

Body-hit dedup mirrors `dedup_java_facts`: `(span.start_byte, kind)` for
references and body hits.

---

## 4. Language gotchas & heuristics

### (a) Top-level functions and properties

Kotlin allows `fun` and `val`/`var` outside any class. The Java extractor
only ever creates `Method`/`Field` under a class parent.

Rule: when a `function_declaration` or `property_declaration` has no
class/object/interface ancestor (the parent walk hits the file root),
emit it with no `parent_id`, `kind = Function` (for funs) or
`kind = Const` (for `val`) / `kind = Static` (for `var`).
For graph routing, `kotlin_symbol_owner_path` returns the file package
unchanged — there is no enclosing class chain to append.

### (b) `companion object`

Companion objects are the JVM-interop equivalent of static members.

Rule: emit the `companion_object` as a `Class` symbol whose `parent_id`
is the host class. Its children (functions, properties) are emitted with
`parent_id = host_class.id` (skip the intermediate companion in the
parent chain) so signatures like `Host.factory()` resolve directly.
Attach attribute `kotlin:companion` to each child and to the companion
symbol itself. Confidence stays `ExactSyntax` / `Confidence::High` for
resolution.

### (c) Extension functions

`fun String.foo()` declares `foo` keyed on a receiver type. The receiver
is parsed as the `function_value_parameters` predecessor node or a
`user_type` child.

Rule:

- Emit as `kind = Function` with `parent_id = None` (top-level).
- Set `language_identity = Some(<receiver_type_text>)` so the cross-file
  resolver can match calls of shape `someString.foo()` against the
  receiver type the call site infers.
- Attribute `kotlin:extension`.
- `Confidence::ExactSyntax` if the receiver is a simple `user_type`
  (resolvable name); `Confidence::Partial` if the receiver is generic,
  nullable (`String?`), nested (`Map.Entry`), or otherwise can't be
  flattened to a single identifier on a syntactic pass.

### (d) `inline` / `reified`

No special modeling. Inline expansion is invisible to a syntactic pass.
Emit a regular function symbol and attach `kotlin:inline` (and
`kotlin:reified` per type parameter) as attributes for downstream
inspection.

### (e) Data classes

Kotlin's compiler generates `copy`, `componentN`, `equals`, `hashCode`,
`toString` for `data class`. The Java compiler-tree oracle does *not*
synthesize them; the JetBrains Kotlin compiler-embeddable oracle *does*
when descriptor mode is on.

Rule for the first PR: **exclude** generated members from the extractor.
Mark the data class with attribute `kotlin:data`. The oracle helper must
suppress generated members to match (`KotlinOracle.kt` must filter
`DescriptorUtils.isSynthesized`). This keeps the symbol-set gates
symmetric. A phase-2 PR can flip on `copy`/`componentN` emission as
`Confidence::Partial` once both sides agree.

### (f) Sealed classes / sealed interfaces

Children declared via `: Parent()` or `: Parent` show up in the child's
`delegation_specifier` list. Emit them as `base:<parent>` attributes on
the child, identical to Java's inheritance handling
(`java_type_inheritance_names`). On the parent, a phase-2 PR can add
`kotlin_sealed_children` lookups; first PR does not.

### (g) Delegated properties

`val x by lazy { ... }` is a `property_declaration` with a
`property_delegate` child.

Rule: emit `x` as a single property symbol. Do not emit the delegate
expression body as a separate symbol. Attribute `kotlin:delegated`.
`Confidence::Partial` because the effective accessor body lives in the
delegate.

### (h) Suspend functions

`suspend fun f()` is a `function_declaration` with a `suspend` modifier
child.

Rule: emit as a regular function, add attribute `kotlin:suspend`.
Otherwise indistinguishable from a normal function.

### (i) Anonymous objects

`object : Runnable { ... }` (expression form) creates an anonymous
local symbol. Emit nothing; treat it like a local block. The named
declaration form is covered by (j).

### (j) `object` declarations

Top-level `object Foo { ... }` declares a singleton. Emit as
`SymbolKind::Class`, attribute `kotlin:object`, `Confidence::ExactSyntax`.
Children emit normally.

### (k) Type aliases

`typealias UserId = String` is a `type_alias` node.

Rule: emit as `kind = SymbolKind::TypeAlias` with `name = "UserId"` and
`language_identity = Some("String")` (or the multi-segment target as a
string). This lets the resolver carry the alias when matching
`UserId.length` to `String.length`.

### (l) Imports

| Kotlin syntax                | `ImportKind` | `alias`        | `is_glob` | `imported_name`             |
| ---------------------------- | ------------ | -------------- | --------- | --------------------------- |
| `import a.b.C`               | `Named`      | `None`         | false     | `Some("C")`                 |
| `import a.b.*`               | `Wildcard`   | `None`         | true      | `None`                      |
| `import a.b.C as X`          | `Named`      | `Some("X")`    | false     | `Some("X")` (alias-bound)   |

Note: Kotlin has no `import static`; the closest equivalent is plain
`import a.b.foo` of a top-level or companion-object member, which the
resolver handles via the same path-prefix logic as Java's static import
because top-level functions, companion members, and `object` members all
present as members of a path.

---

## 5. Per-symbol confidence rules

| Symbol shape                                 | `Confidence`                                                  |
| -------------------------------------------- | ------------------------------------------------------------- |
| Class, interface, enum, object, type alias   | `ExactSyntax`                                                 |
| Top-level function with no receiver          | `ExactSyntax`                                                 |
| Method on class/object/companion             | `ExactSyntax`                                                 |
| Extension fun on a resolvable receiver       | `ExactSyntax`                                                 |
| Extension fun on generic/nullable/nested receiver | `Partial`                                                |
| Property delegated `by` something            | `Partial`                                                     |
| Generated data-class member (if emitted later) | `Partial`                                                   |
| Secondary constructor                        | `ExactSyntax`                                                 |
| Suspend function                             | `ExactSyntax` (the `suspend` flag is an attribute, not a downgrade) |
| Enum entry                                   | `ExactSyntax`                                                 |

Call confidence mirrors Java: `Method` calls land as `CandidateSet`
until resolution, direct constructions as `Heuristic`.

---

## 6. Fixture sketch

Layout under `benchmarks/fixtures/kotlin/semantic-cases/`. Modeled
after the existing Java fixture with a Gradle file, a `vendor/` dir,
and a `src/generated/` dir for fallback-quality coverage.

```
benchmarks/fixtures/kotlin/semantic-cases/
  build.gradle.kts                                — project facts file
  src/main/kotlin/com/example/app/Runner.kt       — top-level + class
  src/main/kotlin/com/example/services/Greeter.kt — sealed interface + impls
  src/main/kotlin/com/example/services/FriendlyGreeter.kt — companion factory
  src/main/kotlin/com/example/util/Strings.kt     — extension fun used cross-file
  src/main/kotlin/com/example/util/Names.kt       — data class + suspend fun
  src/generated/kotlin/com/example/generated/GeneratedGreeter.kt — fallback exclusion
  vendor/com/example/Ignored.kt                   — fallback exclusion
```

Coverage matrix:

| Concern                                | Hit by                                                            |
| -------------------------------------- | ----------------------------------------------------------------- |
| package + named import                 | every file                                                        |
| `import ... as` alias                  | `Runner.kt` imports `FriendlyGreeter as Friendly`                 |
| wildcard import                        | `Names.kt` imports `kotlin.text.*` to confirm Wildcard kind       |
| data class with `copy`                 | `Names.kt` declares `data class Person(val name: String)`         |
| sealed interface + implementations     | `Greeter.kt` declares `sealed interface Greeter`; `FriendlyGreeter` and a local `RudeGreeter` implement it |
| extension function cross-file          | `Strings.kt` `fun String.prepare()`; called from `Runner.kt`      |
| companion-object factory               | `FriendlyGreeter.kt` has `companion object { fun create() = ... }` |
| suspend function call chain            | `Runner.run()` (also `suspend`) calls `Names.fetchDefault()` (suspend) |
| top-level `val`                        | `Names.kt` declares `val GREETING: String = "Hello"`              |
| type alias                             | `Names.kt` declares `typealias Greeting = String`                 |
| object declaration                     | `Strings.kt` declares `object StringOps { ... }`                  |
| Gradle source-set / dependency facts   | `build.gradle.kts` declares `srcDir 'src/main/kotlin'` and an `implementation` dep |
| generated-source exclusion             | `src/generated/kotlin/...` path triggers `generated_exclusion`    |
| vendor exclusion                       | `vendor/` triggers default workspace exclusion                    |

`generated_source_root` in `crates/squeezy-graph/src/languages/java.rs:508-520`
must be extended to recognize Kotlin-specific markers as well, ideally in a
new `kotlin_generated_source_root` so the Java helper stays Java-only:

```text
target/generated-sources/
build/generated/source/
generated-src/
src/generated/kotlin/
```

The implementing PR adds a small `kotlin_source_root_facts` and
`kotlin_configured_source_facts` pair that recognise the
`src/<set>/kotlin/...` layout and Gradle `srcDir 'src/main/kotlin'`
syntax. Maven mode reuses the Java `sourceDirectory` reading.

---

## 7. Real-repo corpus

**Recommendation**: `JetBrains/kotlinx.coroutines`, smoke subset
`kotlinx-coroutines-core/common/src/`.

| Field                | Value                                                                      |
| -------------------- | -------------------------------------------------------------------------- |
| Repo URL             | `https://github.com/Kotlin/kotlinx.coroutines`                             |
| Suggested tag        | `1.10.1` (most recent stable as of writing — verify before pinning)        |
| Smoke fixture path   | `target/benchmark-repos/kotlinx-coroutines-smoke/kotlinx-coroutines-core/common/src` |
| Smoke file count     | ~80 (target the 50–150 band)                                               |
| Why                  | Idiomatic modern Kotlin, multiplatform `expect`/`actual`, heavy use of suspend funs, sealed hierarchies, inline + reified, extension functions on Continuation / CoroutineContext. JetBrains-maintained so style is canonical. |

Add a `kotlin-smoke` entry to `benchmarks/corpus.json` following the
existing `java-smoke` shape and a `kotlinx-coroutines` `full` entry for
phase 2:

```json
{
  "name": "kotlin-smoke",
  "family": "kotlin",
  "language": "kotlin",
  "tier": "smoke",
  "fixture": "benchmarks/fixtures/kotlin/semantic-cases",
  "spec": "benchmarks/specs/kotlin-smoke-queries.json",
  "report": "kotlin/kotlin-smoke.json",
  "ra_lsp_probes": 0
},
{
  "name": "kotlinx-coroutines",
  "family": "kotlin",
  "language": "kotlin",
  "tier": "full",
  "fixture": "target/benchmark-repos/kotlinx-coroutines/kotlinx-coroutines-core/common/src",
  "spec": "benchmarks/specs/empty-queries.json",
  "report": "kotlin/kotlinx-coroutines.json",
  "ra_lsp_probes": 0,
  "no_speed_gate": true,
  "repo": {
    "url": "https://github.com/Kotlin/kotlinx.coroutines",
    "rev": "<pin-to-1.10.1-sha>",
    "checkout": "target/benchmark-repos/kotlinx-coroutines"
  }
}
```

---

## 8. Smoke query spec

File: `benchmarks/specs/kotlin-smoke-queries.json`. Modeled on
`java-smoke-queries.json`. Names below assume the fixture sketch in §6.

```json
{
  "queries": [
    {
      "id": "kotlin-hierarchy",
      "kind": "hierarchy_contains",
      "expected_contains": [
        "Class:Runner",
        "Trait:Greeter",
        "Class:FriendlyGreeter",
        "Class:RudeGreeter",
        "Class:Person",
        "Class:StringOps",
        "Method:run",
        "Method:greet",
        "Method:create",
        "Function:prepare",
        "Function:fetchDefault"
      ]
    },
    {
      "id": "kotlin-class-signature",
      "kind": "signature_search",
      "text": "class Runner",
      "symbol_kind": "Class",
      "expected_contains": ["Class:Runner"]
    },
    {
      "id": "kotlin-data-class-copy",
      "kind": "signature_search",
      "text": "data class Person",
      "symbol_kind": "Class",
      "expected_contains": ["Class:Person"]
    },
    {
      "id": "kotlin-sealed-references",
      "kind": "references_to_symbol",
      "to": "Greeter",
      "expected_contains": ["Greeter"]
    },
    {
      "id": "kotlin-suspend-call-chain",
      "kind": "call_chain",
      "from": "run",
      "to": "fetchDefault",
      "expected_contains": ["run -> fetchDefault"]
    },
    {
      "id": "kotlin-companion-factory-chain",
      "kind": "call_chain",
      "from": "buildDefault",
      "to": "create",
      "expected_contains": ["buildDefault -> create"]
    },
    {
      "id": "kotlin-extension-resolution",
      "kind": "call_chain",
      "from": "run",
      "to": "prepare",
      "expected_contains": ["run -> prepare"]
    },
    {
      "id": "kotlin-import-alias",
      "kind": "reference_search",
      "text": "Friendly",
      "expected_contains": ["FriendlyGreeter"]
    },
    {
      "id": "kotlin-project-facts",
      "kind": "kotlin_project_facts",
      "expected_contains": [
        "gradle:source_root:main:src/main/kotlin",
        "gradle:dependency:implementation:org.jetbrains.kotlinx:kotlinx-coroutines-core:1.10.1"
      ]
    },
    {
      "id": "kotlin-fallback-quality",
      "kind": "fallback_quality",
      "expected_contains": ["generated", "vendor"]
    }
  ]
}
```

The `kotlin_project_facts` query kind is new in this PR. It follows the
shape of `java_project_facts` and is wired through a
`kotlin_build_metadata_provider` helper that recognises
`build.gradle.kts`, `settings.gradle.kts`, `pom.xml`, and
`build.gradle`. Reuse the Gradle/Maven dependency parsers verbatim from
the Java helper — they're file-format-oriented, not language-oriented —
but call them through Kotlin-named wrappers so the family stays
self-contained.

---

## 9. Oracle plan

### Tool

JetBrains **Kotlin compiler embeddable** jar
(`kotlin-compiler-embeddable-<version>.jar`). It's the same artifact
IDE plugins and the JetBrains Kotlin command line use for PSI / BindingContext
extraction. It runs on a JDK 17+ and accepts a source root plus
classpath, returning a fully-typed PSI tree.

Why this and not alternatives:

- `tree-sitter-kotlin` is a parser, not a type-aware oracle — comparing
  squeezy against itself would be circular.
- A Kotlin LSP (e.g. `fwcd/kotlin-language-server`) wraps the same
  compiler but adds LSP overhead. We may add LSP-based navigation
  probes in phase 2 once `kotlin_navigation_accuracy` work begins.
- `kotlinc -Xdump-declarations-to` exists but the JSON shape is
  unstable. PSI traversal in a small helper program is sturdier.

### Helper layout

```
benchmarks/oracle/kotlin/
  KotlinOracle.kt       — single-file PSI walker
  build.sh              — `kotlinc -include-runtime -d kotlin-oracle.jar KotlinOracle.kt`
```

`KotlinOracle.kt` outline:

- `main(args)` takes `<source-root>` (single arg, mirrors `JavaOracle`).
- Constructs a `KotlinCoreEnvironment` with empty classpath
  (declaration-mode is enough — no need for stdlib resolution at the
  symbol-set level).
- Walks every `.kt` and `.kts` file under root via
  `KtFile.declarations` recursion.
- Emits `{"rows": [["<rel>", "<kind>", "<name>"], ...]}` on stdout,
  same shape as `JavaOracleOutput` so we can reuse
  `serde_json::from_slice::<JavaOracleOutput>` *or* declare a
  parallel `KotlinOracleOutput` struct (the spec recommends parallel
  for clarity, against the inert global-coupling concern from §1).
- Kinds emitted: `Class`, `Trait` (for `interface`), `Enum`,
  `Method`, `Function`, `Field`, `Const`, `Variant`, `TypeAlias`.
- Exclusions inside the oracle (so squeezy and oracle agree):
  - skip `KtDeclaration.isLocal`
  - skip `KtParameter`
  - skip generated/synthesized members
    (`DescriptorUtils.isSynthesized`)
  - skip anonymous object expressions
- Names normalised by `normalize_symbol_name` on the squeezy side
  (already done in `collect_java_compiler_tree_symbol_scan`).

### Install in CI

```yaml
- name: Install Kotlin and JDK 17
  if: inputs.language == 'kotlin'
  shell: bash
  run: |
    sudo apt-get update
    sudo apt-get install -y openjdk-17-jdk
    KOTLIN_VERSION="1.9.24"
    curl -sLo /tmp/kotlin.zip \
      "https://github.com/JetBrains/kotlin/releases/download/v${KOTLIN_VERSION}/kotlin-compiler-${KOTLIN_VERSION}.zip"
    sudo unzip -q /tmp/kotlin.zip -d /opt
    echo "/opt/kotlinc/bin" >> "$GITHUB_PATH"
    kotlinc -version
    java -version
```

(Replace `1.9.24` with the rev the oracle was last validated against. Pin
explicitly — `apt-get install kotlin` on Ubuntu installs an older 1.x
that may not match the grammar.)

### Build oracle jar

A `build-oracle-jar` step compiles `KotlinOracle.kt` to a fat jar
before the benchmark runs. Don't check the jar into the repo (it's
~20MB).

```yaml
- name: Build Kotlin oracle jar
  if: inputs.language == 'kotlin'
  shell: bash
  run: |
    cd benchmarks/oracle/kotlin
    bash build.sh
```

### Scan command

```
java -jar benchmarks/oracle/kotlin/kotlin-oracle.jar <root>
```

Implement `collect_kotlin_compiler_tree_symbol_scan(root)` under
`benchmarks/squeezy-graph-bench/src/oracles/kotlin.rs`, mirroring
`collect_java_compiler_tree_symbol_scan` in `oracles/javac.rs` line-for-line:
spawn the jar via `Command`, parse stdout as `KotlinOracleOutput`,
apply `default_oracle_exclusions(root)`, increment
`SymbolKey { file, kind, name }`.

### Exclusion list (oracle-side, restated for symmetry)

The Kotlin oracle excludes:

- locals (`KtDeclaration.isLocal`)
- lambdas and lambda parameters
- implicit `it` parameters
- anonymous objects
- synthetic accessors / `componentN` / `copy` / `equals` / `hashCode` / `toString` generated members
- function parameters and receiver parameters

Squeezy must match: extractor skips lambdas, anonymous objects, and the
auto-generated `data class` members per §4(e).

### Definition/reference probes

Phase 1: declarations only (symbol-set parity), `ra_lsp_probes: 0` in
the corpus entry. No LSP probes wired.

Phase 2: add a `KotlinLsp` client modeled on `RustAnalyzerLsp`
(`benchmarks/squeezy-graph-bench/src/oracles/rust_analyzer.rs`) that
speaks LSP to `fwcd/kotlin-language-server`. Lift
`collect_navigation_accuracy` and adapt for Kotlin.

### Scan-only fallback

If `kotlinc` or `java -jar` fails (binary missing, jar not built,
heap blew up), emit
`status: "Kotlin oracle unavailable: <err>"` and degrade to
`collect_squeezy_symbol_scan` only. This mirrors the
`time_java_oracle_optional` / `collect_java_oracle_accuracy` skip
behaviour at `oracles/javac.rs:15-80`. Wire the Kotlin family into
`benchmarks/squeezy-graph-bench/src/oracles/common_scan.rs` so the
common-scan fallback runs when the language oracle is skipped.

---

## 10. Gate thresholds for first PR

```
precision >= 0.94
recall    >= 0.85
```

Justification:

- Kotlin is mostly static (`val`/`var`/`fun`/`class`) and JVM-bound, so
  declarations land cleanly — comparable to Java's existing floor.
- Extension functions and delegated properties contribute a small
  predictable false-negative slice (data classes' generated members
  are excluded symmetrically). 0.85 recall absorbs that without
  hiding regressions.
- 0.94 precision tracks the Java oracle's observed precision and
  forces the extractor to dedup correctly (the same
  `(span.start_byte, kind)` trick applies to property destructuring
  declarations, which can fire twice on the same node).

Wire into `enforce_gates` (`benchmarks/squeezy-graph-bench/src/gates.rs`)
following the existing `go_oracle` precedent — add a
`report.kotlin_oracle` field, fail when `fp != 0 || fn != 0` only when
`no_speed_gate == false` *and* the smoke tier was requested. Full-tier
real-repo runs keep `no_speed_gate: true` to avoid flapping.

---

## 11. Speed parity target

Per-file parse+extract time within **1.5× of Java's**.

Measurement: the bench harness already records per-language total ms
in `report.squeezy_total_ms`. Add a comparison line to
`benchmarks/scripts/summarize.py` that prints the Kotlin/Java ratio for
the smoke fixture when both are present in the report glob.

If 1.5× is exceeded, the most likely cause is the tree-sitter-kotlin
grammar being slower than tree-sitter-java; profile with
`cargo flamegraph` on the kotlinx-coroutines corpus and pin grammar
version before relaxing the target.

---

## 12. CI matrix entry

Append to `.github/workflows/benchmark-lang.yml` choice options and to
`.github/workflows/benchmark.yml` job list. Within
`benchmark-lang.yml`, the language-specific steps go inside the
existing single job:

```yaml
# In .github/workflows/benchmark-lang.yml, workflow_dispatch.inputs.language.options:
- kotlin

# In .github/actions/setup-bench/action.yml, add steps:
- name: Set up JDK 17 for Kotlin
  if: inputs.language == 'kotlin'
  uses: actions/setup-java@v4
  with:
    distribution: temurin
    java-version: '17'

- name: Install Kotlin compiler
  if: inputs.language == 'kotlin'
  shell: bash
  run: |
    KOTLIN_VERSION="1.9.24"
    curl -sLo /tmp/kotlin.zip \
      "https://github.com/JetBrains/kotlin/releases/download/v${KOTLIN_VERSION}/kotlin-compiler-${KOTLIN_VERSION}.zip"
    sudo unzip -q /tmp/kotlin.zip -d /opt
    echo "/opt/kotlinc/bin" >> "$GITHUB_PATH"

# In the `case` block of the "Show toolchains" step, add:
kotlin)
  java -version
  kotlinc -version
  ;;

# In benchmark-lang.yml, before "Run semantic graph benchmark corpus":
- name: Build Kotlin oracle jar
  if: inputs.language == 'kotlin'
  shell: bash
  run: |
    cd benchmarks/oracle/kotlin
    bash build.sh

# In .github/workflows/benchmark.yml, add a new job:
kotlin-semantic-graph-smoke:
  name: Kotlin semantic graph smoke
  if: ${{ github.event_name != 'workflow_dispatch' || inputs.language == 'kotlin' || inputs.language == 'all' }}
  uses: ./.github/workflows/benchmark-lang.yml
  with:
    language: kotlin
    tier: ${{ github.event_name == 'workflow_dispatch' && inputs.tier || 'smoke' }}
    artifact-suffix: kotlin
    summary-file: __kotlin_summary.md
  continue-on-error: true
```

`continue-on-error: true` on the parent job keeps the rest of the
benchmark matrix green while Kotlin lands. Drop it once the oracle
gates have been holding for two weeks of CI, per the convention this
repo uses for other newly-added languages.

Also extend the `workflow_dispatch.inputs.language.options` list in
`benchmark.yml` (`:32-46`) to include `kotlin`, and add `kotlin` to
the choice options block in `benchmark-lang.yml` (`:29-41`).

---

## Verify-before-merge checklist

- [ ] `cargo search tree-sitter-kotlin` confirms a stable 0.x crate; if
      not, switch to a git pin against `fwcd/tree-sitter-kotlin` with
      the rev SHA captured in `Cargo.toml`.
- [ ] Grammar exposes `LANGUAGE: LanguageFn` matching the
      `tree_sitter_java::LANGUAGE` shape.
- [ ] Grammar license is MIT or Apache-2.0 (run `cargo deny check`).
- [ ] `kotlin-compiler-embeddable` jar version pinned in `build.sh`
      matches the grammar's tracked language version (1.9.x at time of
      writing).
- [ ] CI workflow added with `continue-on-error: true`.
- [ ] No code from `crates/squeezy-parse/src/languages/java.rs` or
      `crates/squeezy-graph/src/languages/java.rs` is `pub(crate)` to
      Kotlin — Kotlin copies the patterns into its own module.
- [ ] Smoke fixture builds (`gradle build` is *not* run in CI; the
      `build.gradle.kts` is only there for fact extraction).
- [ ] Oracle helper compiles and emits valid JSON when run against the
      smoke fixture before merging.
