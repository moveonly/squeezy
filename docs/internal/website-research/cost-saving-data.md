# Cost-Saving Data for Website Use

This note classifies the benchmark and evaluation data that is suitable for
public Squeezy website charts. The safest public claims are narrow: they should
say what was measured, on which suite, and where the result does not generalize.

## Public-Ready Data

These data sets have a checked-in source, reproducible methodology, and enough
scope to graph publicly if the chart copy stays specific.

| Data set | Source paths | What is usable | Public strength | Do not claim |
|---|---|---|---|---|
| Mini real-world cost scoreboard | `docs/internal/eval-findings/mini-vs-codex-realworld.csv`, `docs/internal/eval-findings/cost-wins-fresh-headhead.md`, `docs/internal/eval-findings/realworld-harness/README.md` | 15 language rows comparing Squeezy with graph enabled against Codex on same scenario family, n=3 medians, identical pricing and grader. Current checked-in CSV: 11 wins / 4 losses, 20.3% aggregate lower cost by total spend across rows, 0.81 median ratio, 100% Squeezy recall in every row. | Strong enough for a public chart labeled as this benchmark suite. | Do not say "15/15", "always cheaper", or "beats Codex on every task". Near-boundary losses exist: go 0.99, java 0.96, ruby 1.02; csharp is a clear cost loss at 1.21. |
| Rust semantic graph full-tier symbol accuracy and timing | `docs/internal/BENCHMARKS.md`, `benchmarks/corpus.json` | Five pinned Rust repos, 25,000 deterministic mixed-workload scenarios, comparable-symbol TP/FP/FN all 20,320/0/0, Squeezy total 3,461 ms. rust-analyzer timing succeeded on 4 of 5 repos; excluding bat's local RA failure, Squeezy was about 7.1x faster than RA analysis-stats for the reported timing rows. | Strong enough for public graphing if framed as local release benchmark and comparable declaration symbols. | Do not imply full rust-analyzer replacement, full semantic equivalence, or perfect reference navigation. LSP navigation has known FP/FN and lower recall. |
| Java declaration oracle accuracy | `docs/internal/BENCHMARKS.md`, `benchmarks/corpus.json` | Five external Java repos. Aggregated declaration oracle: TP 107,063, FP 4, FN 8, precision 0.99996, recall 0.99993. | Strong enough for a public accuracy chart if labeled "declaration discovery vs JDK compiler tree oracle". | Do not claim overload, dispatch, annotation processor, reference, or classpath completeness. The docs explicitly limit the Java oracle to declarations plus fixture query checks. |
| Go latest full run declaration accuracy | `docs/internal/BENCHMARKS.md`, `benchmarks/corpus.json` | Five external Go repos after heuristic iteration. Aggregated declaration oracle: TP 29,038, FP 0, FN 0, precision 1.0000, recall 1.0000. Refresh probe reparsed exactly the two edited files on every repo. | Strong enough for public charting as declaration accuracy plus incremental refresh behavior. | Do not compare cold full graph time directly against the Go oracle as equivalent work. Prometheus and etcd are slower than the declaration-only oracle because Squeezy builds references, calls, body hits, and edges too. |
| Smoke fixture baseline JSONs | `benchmarks/baselines/dart.json`, `benchmarks/baselines/ruby.json`, `benchmarks/baselines/scala.json`, `benchmarks/baselines/swift.json` | Small fixture-level graph build vs oracle timing and exact symbol accuracy where reported. Dart 4 ms vs analyzer 5,145 ms, Ruby 3 ms vs Prism 74 ms, Scala 4 ms vs semanticdb 2,267 ms, Swift 10 ms build vs sourcekit document-symbol 838 ms. | Good supporting proof that benchmark plumbing exists across newer languages. | Too small for headline cost-saving or broad performance claims. Treat as smoke-fixture validation, not representative repo performance. |

## Metrics Table for Website Charts

### Mini Real-World Cost

Derived from `docs/internal/eval-findings/mini-vs-codex-realworld.csv`.

| Lang | Squeezy cost | Codex cost | Ratio | Squeezy recall | Verdict | Chart use |
|---|---:|---:|---:|---:|---|---|
| c | $0.0454 | $0.0504 | 0.90 | 100.0% | WIN | Usable |
| cpp | $0.0557 | $0.0689 | 0.81 | 100.0% | WIN | Usable |
| csharp | $0.0636 | $0.0525 | 1.21 | 100.0% | LOSS | Show if using all rows; do not hide losses |
| dart | $0.1049 | $0.1802 | 0.58 | 100.0% | WIN | Usable |
| go | $0.0479 | $0.0486 | 0.99 | 100.0% | LOSS | Boundary / caveated |
| java | $0.1441 | $0.1499 | 0.96 | 100.0% | LOSS | Boundary / caveated |
| js | $0.0552 | $0.0650 | 0.85 | 100.0% | WIN | Usable |
| kotlin | $0.0271 | $0.0416 | 0.65 | 100.0% | WIN | Usable |
| php | $0.0261 | $0.0418 | 0.62 | 100.0% | WIN | Usable |
| python | $0.0155 | $0.0193 | 0.81 | 100.0% | WIN | Usable |
| ruby | $0.0617 | $0.0607 | 1.02 | 100.0% | LOSS | Boundary / caveated |
| rust | $0.0278 | $0.0355 | 0.78 | 100.0% | WIN | Usable |
| scala | $0.0202 | $0.0611 | 0.33 | 100.0% | WIN | Usable |
| swift | $0.0134 | $0.0181 | 0.74 | 100.0% | WIN | Usable |
| ts | $0.0378 | $0.0424 | 0.89 | 100.0% | WIN | Usable |

Aggregate from the checked-in CSV:

| Metric | Value | Notes |
|---|---:|---|
| Rows | 15 | 15 language scenarios |
| Wins | 11 | Strict win means Squeezy recall holds and cost is at least 5% lower |
| Losses | 4 | csharp, go, java, ruby |
| Aggregate cost delta | 20.3% lower | Sum of Squeezy costs vs sum of Codex costs across rows |
| Median cost ratio | 0.81 | Lower is cheaper for Squeezy |
| Average cost ratio | 0.81 | Unweighted by dollars |

Recommended exact public wording:

> On our 15-language real-world graph benchmark using gpt-5.4-mini, Squeezy's graph-enabled agent used 20% less total model spend than the Codex baseline while preserving 100% measured recall across the suite. It was cheaper on 11 of 15 language tasks; four tasks were at parity or more expensive.

Shorter option for a chart caption:

> 20% lower total model spend across 15 measured code-navigation tasks, with all losses shown.

### Semantic Graph Accuracy

Derived from `docs/internal/BENCHMARKS.md` and baseline JSONs.

| Area | Corpus | Accuracy metric | Timing metric | Public chart use | Required caveat |
|---|---|---:|---:|---|---|
| Rust comparable declarations | ripgrep, fd, bat, tokio, serde | 20,320 TP / 0 FP / 0 FN | 3,461 ms Squeezy over 25,000 mixed scenarios | Accuracy plus timing chart | Comparable declarations only; RA locals, fields, variants excluded; reference navigation is not perfect. |
| Java declarations | junit5, mockito, guava, retrofit, picocli | 107,063 TP / 4 FP / 8 FN | Per-repo Squeezy totals range 502 ms to 20,804 ms | Accuracy chart | Declaration-only against JDK compiler tree oracle; no overload/dispatch/classpath completeness claim. |
| Go declarations | gin, cobra, prometheus, etcd, zap | 29,038 TP / 0 FP / 0 FN | 8,464 ms total Squeezy full graph across five repos | Accuracy plus incremental refresh chart | Squeezy full graph includes more work than the Go declaration-only oracle. |
| C# smoke fixture | semantic-cases | 32 symbol TP / 0 FP / 0 FN; 2 edge TP / 0 FP / 0 FN | 6 ms Squeezy vs 621 ms dotnet build | Small supporting stat | Smoke fixture only; external C# corpus has materially lower precision/recall. |
| Dart smoke fixture | semantic-cases | 21 TP / 0 FP / 0 FN | 4 ms Squeezy vs 5,145 ms analyzer | Supporting stat only | Cold analyzer startup dominates; smoke fixture only. |
| Ruby smoke fixture | semantic-cases | 12 TP / 0 FP / 0 FN | 3 ms Squeezy vs 74 ms Prism | Supporting stat only | Smoke fixture only. |
| Scala smoke fixture | semantic-cases | 25 TP / 0 FP / 0 FN | 4 ms Squeezy vs 2,267 ms semanticdb | Supporting stat only | Smoke fixture only. |
| Swift smoke fixture | semantic-cases | Navigation probes reported, no TP/FP/FN in JSON | 10 ms Squeezy build vs 838 ms sourcekit symbols | Supporting stat only | Platform-specific local run; no symbol TP/FP/FN baseline JSON. |

Recommended exact public wording:

> Squeezy builds a local tree-sitter semantic graph and checks it against compiler or language-service oracles in benchmark mode. On the current Rust full-tier declaration benchmark, the comparable declaration set matched rust-analyzer symbols with 20,320 true positives and no false positives or false negatives across five pinned repositories.

Alternative broader wording:

> The benchmark suite validates Squeezy's graph against language-specific oracles, including rust-analyzer, JDK compiler trees, Roslyn syntax, Go AST parsing, Prism, SourceKit, and Dart analyzer. These oracles are used for testing; production navigation stays local and tree-sitter based.

## Candidate Chart Specs

| Chart | Data | Encoding | Include | Avoid |
|---|---|---|---|---|
| Cost ratio by language | Mini CSV | Horizontal bars sorted by `ratio`, vertical line at 1.0, color wins vs losses, label ratio and recall | All 15 rows, including csharp/go/java/ruby losses | Do not crop the x-axis below 1.0 or omit losses. |
| Total spend comparison | Mini CSV | Two bars: sum Squeezy cost vs sum Codex cost; annotate 20.3% lower total spend | Method note: n=3 medians, same scenarios, same grader/pricing | Do not present as provider-independent savings. |
| Accuracy by language family | BENCHMARKS tables and baseline JSON | Small multiples with TP/FP/FN stacked or precision/recall dots | Rust/Java/Go full corpora separate from smoke fixtures | Do not merge smoke fixtures and external full corpora into one "average accuracy" number. |
| Rust graph timing | BENCHMARKS Rust local results | Per-repo grouped bars: Squeezy total vs rust-analyzer where available; separate refresh markers | Show bat RA failed locally | Do not include bat in RA speedup aggregate. |
| Incremental refresh | Rust and Go tables | Dot plot of refresh after two edits | Rust refresh values and Go "exactly two edited files" note | Do not imply all languages have identical incremental behavior unless backed by reports. |
| Graph vs oracle smoke fixtures | baseline JSONs | Table or compact spark bars by language | Dart/Ruby/Scala/Swift smoke-fixture numbers | Do not headline these as real-world repo benchmarks. |

## Data to Keep Internal or Caveated

| Data | Source paths | Status | Reason |
|---|---|---|---|
| Haiku real-world cost CSV as a headline | `docs/internal/eval-findings/haiku-vs-cc-realworld.csv`, `docs/internal/eval-findings/realworld-scoreboard-methodology.md`, `docs/internal/eval-findings/cost-wins-fresh-headhead.md` | Internal or heavily caveated | Current CSV is 8 wins / 7 losses and aggregate cost is 2.7% higher for Squeezy. Some rows in narrative docs use best-of-3 or recall-parity variants that do not exactly match the checked-in CSV. Good for internal diagnosis, not a public savings headline. |
| "15/15" Mini claim | `docs/internal/eval-findings/cost-wins-fresh-headhead.md`, `docs/internal/eval-findings/measurement-integrity-fixes.md` | Do not publish | The fresh head-to-head doc says the old committed 15/15 mini board used stale, more expensive rival baselines and that current fresh rivals are cheaper on several read-heavy tasks. |
| Graph-cost tax and root-cause decomposition | `docs/internal/eval-findings/graph-cost-wins-report.md` | Internal | Useful engineering analysis, but the same document flags noise-dominated verdicts, blocked or corrected remeasurement, stale/grader issues, and benchmark-specific mechanics. |
| C# full-tier external accuracy | `docs/internal/BENCHMARKS.md` | Caveated internal unless used as "known gap" | Five-repo external precision ranges from 0.6596 to 0.9224 and recall from 0.6805 to 0.9255. Useful for honesty, not a public accuracy headline. |
| Rust LSP reference/definition oracle numbers | `docs/internal/BENCHMARKS.md` | Internal or roadmap | The docs explicitly say navigation FP/FN numbers are intentionally not zero and mention package resolution, cfg, trait dispatch, macro expansion, and toolchain noise. |
| Per-fix deltas such as graph-packet slimming 20-30% | `docs/internal/eval-findings/cost-wins-fresh-headhead.md` | Supporting copy only, not chart headline | The fix is generic and tested, but the percentage is packet-level and does not directly equal end-to-end session savings. |

## Caveats to Publish Near Charts

- The cost benchmark measures one suite: 15 real-world code-navigation tasks, one prompt per language, n=3 median runs.
- Squeezy cost includes parent and delegate subagent spend.
- The Mini comparison uses identical pricing and grader on both sides in the current harness.
- A "win" requires equal-or-better recall and at least 5% lower cost; near-parity rows are shown as losses.
- Compiler and language-service oracles are benchmark aids only. Production navigation uses local tree-sitter graph analysis, not LSP/compiler services.
- Declaration accuracy does not imply complete runtime dispatch, macro expansion, overload resolution, build-tag/cfg awareness, generated-code flow, or full reference recall.

## Recommended Public Wording

Use:

> Squeezy reduces model spend by doing code navigation locally before asking the model to reason. In our 15-language Mini benchmark, the graph-enabled agent used 20% less total model spend than a Codex baseline at 100% measured recall, with every language row and loss shown.

Use:

> The semantic graph is validated against language-specific oracles in CI and release benchmarks, while production navigation stays local and tree-sitter based.

Use:

> On five pinned Rust repositories, Squeezy matched rust-analyzer's comparable declaration symbols with 20,320 true positives and no false positives or false negatives in the recorded full-tier run.

Avoid:

> Squeezy cuts coding-agent cost by 50%.

Avoid:

> Squeezy always beats Codex or Claude Code.

Avoid:

> Squeezy has perfect code understanding.

Avoid:

> The graph is always cheaper than grep.

## Source Index

| Source | Used for |
|---|---|
| `docs/internal/eval-findings/mini-vs-codex-realworld.csv` | Current Mini row-level costs, recall, ratios, verdicts |
| `docs/internal/eval-findings/haiku-vs-cc-realworld.csv` | Haiku row-level diagnostic data and reasons not to headline Haiku savings |
| `docs/internal/eval-findings/cost-wins-fresh-headhead.md` | Corrected measurement protocol, current board narrative, residual-loss analysis |
| `docs/internal/eval-findings/realworld-scoreboard-methodology.md` | Scenario definition, n>=3 medians, frozen baseline caveats, delegate cost inclusion |
| `docs/internal/eval-findings/measurement-integrity-fixes.md` | Grader/prompt drift fixes and why older boards require caution |
| `docs/internal/eval-findings/graph-cost-wins-report.md` | Internal root-cause analysis and caveats about variance/noise |
| `docs/internal/BENCHMARKS.md` | Semantic graph benchmark methodology, oracle limitations, Rust/Java/Go/C# tables |
| `benchmarks/corpus.json` | Pinned benchmark repo manifest and scenario counts |
| `benchmarks/baselines/*.json` | Smoke fixture timing and symbol accuracy baselines for Dart/Ruby/Scala/Swift |
