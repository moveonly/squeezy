# Semantic Graph Benchmarks

The semantic graph benchmark suite validates Squeezy's parser-backed graph
against deterministic fixture queries and slower language-specific oracles. The
oracles are benchmark and CI aids only. Production navigation stays local and
tree-sitter based; it does not call compilers, LSP servers, analyzers, Node,
Composer, Ruby, Dart, or other language runtimes during graph queries.

Language coverage, parser families, oracle identifiers, mixed-workload support,
fixtures, specs, and full-tier corpus names are summarized in
[`../../crates/squeezy-skills/external-docs/LANGUAGES.md`](../../crates/squeezy-skills/external-docs/LANGUAGES.md).
The language-specific implementation contracts for Kotlin, Scala, PHP, Ruby,
Swift, and Dart live under [`lang-specs/`](lang-specs/).

## Layout

```text
benchmarks/
  corpus.json                       # smoke/full corpus manifest and pinned repos
  fixtures/<family>/semantic-cases/ # small deterministic smoke fixtures
  specs/*-smoke-queries.json        # expected query results and fallback checks
  specs/empty-queries.json          # external-corpus no-op query spec
  oracle/                           # checked-in oracle programs/scripts
  oracle-helpers/                   # dependency-managed oracle helpers
  scripts/check_languages_doc.py    # live registry/doc consistency check
  scripts/summarize.py              # report summary renderer
  squeezy-graph-bench/              # Rust benchmark CLI
```

Fixtures exist for every supported family: Rust, Python, Java, Kotlin, Scala,
Go, C, C++, C#, JavaScript/TypeScript, PHP, Ruby, Swift, and Dart. Smoke specs
include a `fallback_quality` query so generated/vendor paths and unsupported
fallback evidence cannot silently become graph-confident answers.

## CLI

List the live language and oracle registries:

```sh
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- --list-languages
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- --list-oracles
```

Run the corpus entry point:

```sh
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --corpus benchmarks/corpus.json \
  --family all \
  --tier smoke \
  --report-dir target/semantic-graph-benchmark
```

Run one fixture directly:

```sh
cargo run --release --manifest-path benchmarks/squeezy-graph-bench/Cargo.toml -- \
  --language php \
  --fixture benchmarks/fixtures/php/semantic-cases \
  --spec benchmarks/specs/php-smoke-queries.json \
  --report target/semantic-graph-benchmark/php-smoke.json
```

Render summaries from JSON reports:

```sh
python3 benchmarks/scripts/summarize.py \
  --language all \
  --report-glob "target/semantic-graph-benchmark/**/*.json" \
  --output target/semantic-graph-benchmark/summary.md
```

Supported corpus families are `all`, `rust`, `python`, `java`, `go`,
`c-family`, `c`, `cpp`, `csharp`, `js-ts`, `javascript`, `typescript`,
`kotlin`, `php`, `ruby`, `scala`, `swift`, and `dart`. Supported direct
`--language` values are `rust`, `python`, `java`, `kotlin`, `scala`, `c`,
`cpp`, `csharp`, `go`, `javascript`, `typescript`, `js-ts`, `php`, `ruby`,
`swift`, and `dart`.

Direct runs also support `--mixed-repo <path>`, `--mixed-iterations <n>`,
`--ra-lsp-probes <n>`, `--oracle-files <n>`, and `--no-speed-gate`. Mixed
workload generation currently runs for Rust, C, C++, C#, Go, JavaScript,
TypeScript, and PHP. Other families can still run smoke/full corpus cases, but
their mixed workload status is reported as unsupported.

## Corpus

`benchmarks/corpus.json` is the reproducible benchmark manifest. Each case
declares the family, benchmark language, tier, fixture/spec/report paths,
optional mixed-workload repo and iteration cap, optional oracle probe limits,
speed-gate behavior, and pinned external repository checkout.

The smoke tier always uses small checked-in fixtures before any external corpus
variance. Full-tier cases clone pinned upstream repositories from the manifest
and usually run in reporting mode with `no_speed_gate = true` when compiler or
oracle behavior is expected to vary across machines.

Current full-tier external corpus coverage:

| Family | Full-tier cases |
|---|---|
| Rust | ripgrep, fd, bat, tokio, serde |
| Python | requests, flask, click, black, fastapi |
| Java | junit5, mockito, guava, retrofit, picocli |
| Kotlin | kotlinx-coroutines |
| Scala | utest |
| Go | gin, cobra, prometheus, etcd, zap |
| C/C++ | redis, curl, sqlite, protobuf, nlohmann_json |
| C# | newtonsoft_json, dapper, automapper, polly, serilog |
| JS/TS | vite, redux, axios, express, prettier |
| PHP | symfony-console |
| Ruby | sinatra |
| Swift | swift-nio |
| Dart | smoke only |

The table above is limited to the pinned semantic-graph corpus in
`benchmarks/corpus.json`. The separate graph-vs-no-graph agent eval scenarios
under `crates/squeezy-eval/fixtures/scenarios/benchmarks/` include additional
real-world coverage, including Scala `akka/akka` and Dart `flutter/flutter`.

## CI

Benchmark CI is consolidated into:

- `.github/workflows/benchmark.yml`: pull request, push, and manual
  orchestrator with one job per language family.
- `.github/workflows/benchmark-lang.yml`: reusable `workflow_call` runner used
  by every family job.
- `.github/actions/setup-bench/action.yml`: shared toolchain setup and Cargo
  cache.
- `benchmarks/scripts/check_languages_doc.py`: verifies the user-facing
  language coverage matrix against live `--list-languages` and `--list-oracles`
  output.
- `benchmarks/scripts/summarize.py`: writes the GitHub step summary.

Pull requests run the language docs check and use path filters to run touched
language jobs plus any shared graph/parser/workspace/benchmark changes. Pushes
to `main` and `benchmark-full/**` run the orchestrated workflow. Manual
`workflow_dispatch` accepts `language=all|rust|python|java|go|c-family|csharp|js-ts|kotlin|swift|ruby|php|scala|dart`
and `tier=smoke|full`.

The reusable runner installs only the toolchains needed by the selected family:
Python, Go, .NET, JDK/Kotlin, clang, Scala/Coursier, TypeScript, PHP/Composer,
Ruby/Prism, Swift/SourceKit-LSP, or Dart/analyzer helper dependencies. Reports
and summaries are uploaded as `semantic-graph-benchmark-*` artifacts.

## Gates

Every run fails if required fixture query results are missing. Unless
`--no-speed-gate` is set, the run also fails when the graph build/query time is
not faster than the selected validation timing. Refresh probes fail when a
two-file edit reparses more files than it edited.

Language accuracy gates in `benchmarks/squeezy-graph-bench/src/gates.rs`:

| Family | Gate |
|---|---|
| Go | smoke-tier oracle FP and FN must both be zero when speed gates are active |
| Kotlin | precision >= 0.94 and recall >= 0.85 when the oracle jar ran |
| PHP | precision >= 0.92 and recall >= 0.80 when nikic/PHP-Parser ran |
| Ruby | precision >= 0.90 and recall >= 0.75; scan-only mode self-compares and must be read through the report mode/status |
| Scala | precision >= 0.90 and recall >= 0.75 only when SemanticDB ran end to end |
| Swift | symbol precision >= 0.92 and recall >= 0.80 when comparable symbols exist; definition precision >= 0.85 when probes run; reference precision >= 0.80 only with at least five reference emissions |
| Dart | precision >= 0.93 and recall >= 0.85 only in analyzer mode |

Rust, Python, Java, C/C++, C#, and JS/TS oracle reports are still recorded and
summarized, but the current hard gates are fixture query correctness, speed
where enabled, refresh behavior, and the language-specific checks above.

## Oracles

Oracles compare Squeezy's tree-sitter graph with a slower language-specific
source of truth. They are intentionally narrower than production navigation:
most compare declaration symbols and selected syntactic edges, not full runtime
dispatch or type-checker-equivalent navigation.

| Family | Oracle behavior |
|---|---|
| Rust | `rust-analyzer symbols`, optional analysis timing, and sampled LSP definition/reference probes controlled by `--ra-lsp-probes` |
| Python | CPython `ast` declaration scan; no code execution or import side effects |
| Java | JDK compiler tree declaration scan; query specs gate navigation minima |
| Kotlin | generated JetBrains compiler-embeddable PSI jar; skipped if Java or the jar is unavailable |
| Scala | SemanticDB protobufs emitted by `scalac -Xsemanticdb`; scan-only fallback suppresses the precision/recall gate |
| Go | benchmark-only Go parser/AST/type scan for top-level declarations |
| C/C++ | sampled clang AST JSON; unparseable files are excluded from Squeezy FP accounting |
| C# | Roslyn declaration and syntactic inheritance/interface edge scan |
| JS/TS | TypeScript compiler API plus optional sampled navigation probes |
| PHP | Composer-backed nikic/PHP-Parser declaration scan |
| Ruby | Prism declaration scan with scan-only self-compare fallback |
| Swift | SourceKit-LSP `documentSymbol` and optional definition/reference probes, with syntactic scan fallback |
| Dart | `package:analyzer` helper with scan-only fallback when Dart/analyzer mode is unavailable |

Generated/vendor exclusions and language-specific comparison normalizations live
with the oracle code in `benchmarks/squeezy-graph-bench/src/oracles/`. The
language contracts in `docs/internal/lang-specs/` describe the accepted recall
gaps for the newest language families.

## Reports

Each JSON report includes wall-clock timing, validation/oracle status,
query-level missing/extra results, fallback quality, low-confidence edge counts,
grep-baseline counts, deterministic tool/cost metrics
(`estimated_usd_micros = 0`), optional mixed-workload summaries, refresh probe
results, and oracle precision/recall where available.

Use summaries for a quick matrix view, but keep raw JSON artifacts as the audit
record for precision/recall, fallback, and skipped-oracle details.
