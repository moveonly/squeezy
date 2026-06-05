# Website Eval and Validation Research

Status: current-tree research for website copy. Last checked in this checkout on
2026-06-05. This file is planning input only; do not treat it as a published
benchmark page.

## Evidence Checked

- `crates/squeezy-eval`: agent-driven scenario runner, capture pipeline,
  findings, diff/check commands, TUI capture, tickets, mock provider, and
  scenario fixtures.
- `crates/squeezy-harness`: deterministic task harness, mock/live runners,
  replay mode, planner-probe comparison, and grep baseline.
- `benchmarks/squeezy-graph-bench`: semantic graph benchmark CLI, reports,
  gates, mixed workload generation, language oracles, and summary output.
- `benchmarks/corpus.json`, `benchmarks/specs/*.json`,
  `benchmarks/baselines/*.json`: pinned corpus, query specs, smoke baselines,
  oracle limits, and report paths.
- `docs/internal/BENCHMARKS.md`, `docs/internal/VALIDATION_HARNESS.md`,
  `docs/internal/EVAL_HARNESS.md`: current runbooks and caveats.
- `docs/internal/eval-findings`: UX findings, real-world graph-vs-no-graph
  CSVs, measurement-integrity notes, and methodology docs.

No network sources were used.

## High-Level Finding

Squeezy has unusually strong internal evidence infrastructure for a coding
agent, and the website can use that as a trust story:

- deterministic CI validation for small tasks with no model spend,
- live-agent eval scenarios that produce traces, frames, findings, tickets, and
  redacted bug bundles,
- semantic graph benchmarks with pinned corpora, language-specific oracles,
  query specs, fallback accounting, mixed workloads, and raw JSON artifacts,
- internal measurement notes that explicitly call out stale baselines, noisy
  rows, reporting-only lanes, and failed claims.

The strongest public angle is not "we have perfect benchmarks." It is:

> Squeezy publishes what it measured, keeps production navigation separate from
> oracle tooling, and preserves enough artifacts to audit failures instead of
> hiding them behind aggregate scores.

## What Is Worth Promoting

| Theme | Implemented evidence | Public-safe website angle | Caveat |
|---|---|---|---|
| Deterministic validation | `squeezy-harness` runs task TOMLs against mock OpenAI/Anthropic traces, replay tapes, planner-probe variants, and grep baselines. CI uses deterministic modes only. | "Core agent behavior is checked with deterministic fixtures before any live-model spend." | Do not imply all coding behavior is deterministic. Live provider runs remain opt-in and variable. |
| Live-agent evals | `squeezy-eval run` drives the real agent loop against local, snapshot, or GitHub workspaces and emits `run.json`, `trace.jsonl`, `frames.jsonl`, `findings.jsonl`, tickets, and a redacted session bundle. | "Exploratory QA runs leave inspectable traces, rendered frames, costs, tool calls, findings, and repro bundles." | Some scenarios use real providers and may be blocked by keys, quotas, latency, or model variance. |
| Offline eval mode | `squeezy-eval` supports `[squeezy] provider = "mock"` while still exercising workspace tools, approvals, redaction, telemetry, frames, findings, and tickets. | "The eval harness can test the agent plumbing offline, without API keys." | Mock evals validate harness and tool plumbing, not model quality. |
| Scenario corpus | Current tree has 263 eval scenarios, including 60 targeted graph-vs-no-graph scenarios and 30 natural real-world graph-vs-no-graph scenarios. | "A growing scenario corpus covers TUI workflows, slash commands, MCP, approvals, prompt queueing, streaming, git surfaces, and graph-vs-no-graph comparisons." | Scenario count is a moving implementation metric. Use only if the site can be refreshed with the same checkout. |
| Findings discipline | Eval findings have stable `rule_id`s for duplicate tools, repeated failures, unsupported slash commands, approval misses, stop-with-intent/no-tool, expectations, finish reasons, token ceilings, and tool errors. | "Runs are machine-checked for common regressions before a human reads the transcript." | Auto-findings are regression signals, not a complete bug oracle. |
| Diff and CI checks | `squeezy-eval diff` compares run directories. `squeezy-eval check` can gate a scenario directory with fail policies, JUnit output, concurrency, and input-token baselines. | "Eval runs can be compared and gated, including token-regression checks." | Public copy should avoid implying every scenario is cheap enough for every PR. |
| Semantic graph benchmarks | `squeezy-graph-bench` has smoke/full tiers, pinned corpus entries, query specs, mixed workloads, raw reports, build-phase timing, fallback quality, answer quality, graph metrics, and oracle reports. | "Graph claims are backed by benchmark reports, not screenshots." | Some full-tier cases are reporting-oriented or have disabled speed gates. |
| Oracle separation | Production graph navigation is tree-sitter/local-analysis based; rust-analyzer, JDK compiler trees, Roslyn, Go AST, clang, TypeScript, Prism, SourceKit, Dart analyzer, etc. are benchmark oracles. | "Language servers and compilers are used to measure Squeezy, not as hidden runtime dependencies." | Do not say Squeezy replaces these tools completely. |
| Fallback honesty | Benchmarks report unsupported files, excluded files/dirs/bytes, fallback rates, low-confidence edges, and grep-baseline query counts. Smoke specs include fallback-quality checks for generated/vendor paths. | "Ignored, generated, vendor, unsupported, and low-confidence paths remain visible in reports." | Do not frame fallback as semantic understanding. |
| Incremental refresh | Benchmark gates fail if a refresh probe reparses more files than it edited; mixed workloads also check refresh probes. Go docs report exactly two reparsed files after two edits in the latest full run. | "Incremental behavior is measured, not assumed." | Do not generalize one language's exact refresh result to every language without current reports. |

## Public-Safe Data

These data points are reasonable for website planning if the final page includes
method notes and shows losses/caveats.

| Data | Source | Safe use | Required wording constraint |
|---|---|---|---|
| Eval scenario inventory | `crates/squeezy-eval/fixtures/scenarios` | Current-tree count: 263 scenarios; 60 targeted benchmark scenarios; 30 natural real-world benchmark scenarios. | Say "in this checkout" or refresh the count during site update. |
| Deterministic harness modes | `docs/internal/VALIDATION_HARNESS.md` | CI modes: mock OpenAI, mock Anthropic, planner-probe, planner-probe-no-planner, grep-baseline. | State that costly live provider runners are opt-in and not enabled by default. |
| Eval artifact schema | `docs/internal/EVAL_HARNESS.md` | `run.json`, `trace.jsonl`, `frames.jsonl`, `findings.jsonl`, and `tickets/` are public-safe as architecture artifacts. | Do not publish raw local run logs unless redacted; use schema examples or generated demo data. |
| Mini real-world scoreboard | `docs/internal/eval-findings/mini-vs-codex-realworld.csv` plus methodology docs | 15 rows, 11 wins / 4 losses, all rows shown; existing cost research recommends "20% lower total model spend" for this specific suite. | Label as one 15-language benchmark suite using `gpt-5.4-mini`; do not say always cheaper or 15/15. |
| Haiku real-world scoreboard | `docs/internal/eval-findings/haiku-vs-cc-realworld.csv` | Useful internal diagnostic data and maybe a future transparent "still improving" engineering note. | Do not use as a savings headline; current rows include many losses and methodology docs describe variance. |
| Benchmark specs | `benchmarks/specs/*.json` | 15 smoke query spec files across supported language families. | Specs are fixture gates, not real-world accuracy claims by themselves. |
| Baseline JSONs | `benchmarks/baselines/{dart,ruby,scala,swift}.json` | Supporting proof for smoke fixture timing/oracle plumbing. | Too small for headline performance claims. |
| Rust/Java/Go full-tier tables | `docs/internal/BENCHMARKS.md`, `benchmarks/corpus.json` | Good candidates for accuracy charts when framed as declaration or comparable-symbol oracle results on pinned repos. | Preserve per-language scope: declaration-only, comparable-symbol only, or reporting-oriented where applicable. |
| Raw benchmark reports/artifacts | `BenchmarkReport` schema and workflow docs | Strong trust story: JSON reports include corpus case, tool metrics, answer quality, fallback quality, graph metrics, oracle accuracy, refresh probes, and mixed workload data. | Website should link to or publish curated reports only after confirming no local paths/secrets/proprietary repo data. |

## Caveats To Keep Visible

- Production navigation is local tree-sitter graph analysis. Compiler, LSP,
  language-service, runtime, and AST tools are benchmark oracles or explicit
  verification tools, not hidden production dependencies.
- Declaration accuracy does not mean complete overload resolution, dynamic
  dispatch, macro expansion, reflection, generated-code flow, framework magic,
  build-tag/cfg equivalence, or full reference recall.
- Some language oracles degrade gracefully when a local toolchain is missing.
  Public copy should say "validated against language-specific oracles where
  available" rather than imply every machine runs every oracle.
- Some full-tier benchmark entries are reporting-oriented with `no_speed_gate`,
  disabled fixture speed gates, sampled oracle files, or documented toolchain
  failures. Keep those rows in charts or avoid the headline.
- `benchmarks/corpus.json` currently contains at least one PHP full-tier entry
  with a `TBD` pinned revision note. Avoid any public claim that depends on
  that exact Symfony full-tier case until the pin is resolved.
- Eval `frames.jsonl` and `trace.jsonl` can contain local paths, prompts, tool
  previews, cost estimates, and evidence hashes. Publish schemas or redacted
  demo runs, not arbitrary internal run artifacts.
- Cost numbers are model, provider, prompt, and date dependent. They are useful
  when tied to a frozen benchmark suite, not as general billing promises.
- The real-world scoreboard methodology references `/tmp` scripts and raw
  external baseline files. Before publication, regenerate or snapshot the
  derivation into repo-owned artifacts.
- Haiku/Claude Code comparison data should remain internal or heavily caveated
  until the methodology and row verdicts are refreshed into one consistent
  checked-in source.
- The scenario count, benchmark rows, and local timing numbers can drift. A
  public page should either pin a release commit or regenerate the data during
  release.

## Suggested Website Sections

### Evidence Ladder

Explain the validation layers from cheapest and most deterministic to broadest:

1. `squeezy-harness`: deterministic task fixtures, mock provider streams,
   replay tapes, planner-probe comparison, grep baseline.
2. `squeezy-eval` mock scenarios: real agent loop and tool plumbing without
   provider spend.
3. `squeezy-eval` live scenarios: model-in-the-loop exploratory QA with traces,
   frames, costs, findings, tickets, and redacted bundles.
4. `squeezy-graph-bench`: graph accuracy/performance against pinned corpora,
   query specs, oracles, fallback accounting, and refresh probes.
5. Cost and real-world scoreboards: curated scenario families with frozen
   baselines, medians, recall checks, and visible losses.

This can be a simple vertical diagram titled "How Squeezy earns a claim."

### Reproducible Graph Benchmarks

Use the benchmark infrastructure as a trust-builder:

- pinned corpus manifest,
- smoke and full tiers,
- language-specific query specs,
- graph vs grep baseline counts,
- build-phase timing,
- fallback and low-confidence rates,
- raw JSON report artifacts,
- oracle precision/recall with limitations attached.

The website copy should emphasize the discipline more than any single perfect
number.

### Eval Run Anatomy

Show a sanitized example of one eval run:

- `run.json`: scenario, provider, model, totals, cost, finding count.
- `frames.jsonl`: assistant text, tool calls, tool hashes, styled TUI lines,
  token totals, finish reason.
- `findings.jsonl`: stable rules such as `duplicate_tool_call`,
  `unsupported_slash_command`, or `expect_finish_reason`.
- `tickets/`: markdown/JSON ticket plus redacted session bundle.

This is a strong developer-trust section because it says how regressions are
found and handed to the next engineer.

### Honest Cost Benchmark

If using the Mini real-world CSV, show every language row:

- horizontal bar chart of Squeezy/Codex cost ratio,
- vertical line at 1.0,
- color wins and losses,
- label recall,
- annotate the aggregate "20% lower total model spend" only for this suite.

Do not hide csharp, go, java, or ruby losses from the checked-in CSV.

### Accuracy With Scope

Use grouped cards or small multiples:

- Rust comparable declarations on five pinned repos.
- Java declaration discovery vs JDK compiler tree oracle.
- Go declaration discovery and two-file refresh behavior.
- Smoke baselines for newer languages as "fixture validation", not broad
  real-world proof.

Every chart should carry its scope in the subtitle.

## Chart Ideas

| Chart | Data source | Why it helps | Design constraint |
|---|---|---|---|
| Evidence ladder | Harness/eval/benchmark docs | Shows validation discipline without overclaiming. | Avoid implying all layers run on every PR. |
| Eval artifact anatomy | `EVAL_HARNESS.md` schemas | Makes trust concrete: traces, frames, findings, tickets. | Use sanitized demo data only. |
| Scenario coverage strip | Scenario file counts by directory/theme | Shows breadth of UX, slash, MCP, approvals, graph-vs-no-graph coverage. | Counts must be regenerated at publish time. |
| Cost ratio by language | Mini CSV | Transparent cost claim with wins and losses visible. | Include all rows and a 1.0 parity line. |
| Benchmark signal matrix | `BenchmarkReport` fields | Highlights timing, accuracy, fallback, graph size, refresh, mixed workload signals. | This is a trust table, not a performance ranking. |
| Accuracy dots by corpus | `BENCHMARKS.md`, raw reports | Shows precision/recall with language-specific scopes. | Do not average smoke fixtures with full corpora. |
| Incremental refresh dot plot | Rust/Go benchmark tables and reports | Supports "incremental graph" credibility. | Say which languages and which run produced the data. |

## Public Copy Candidates

Use:

> Squeezy's graph claims are tested with pinned corpora, language-specific
> oracles, query specs, fallback accounting, and raw JSON benchmark reports.

Use:

> The production navigation path stays local and tree-sitter based. Compilers
> and language servers are used in benchmark mode to measure what the graph gets
> right and what it still misses.

Use:

> Eval runs preserve traces, rendered frames, tool-call hashes, costs, findings,
> and ticket bundles so regressions can be reproduced instead of hand-waved.

Use:

> Deterministic mock and replay harnesses run without provider keys or network
> access; live provider checks are explicit and opt-in.

Avoid:

> Perfect semantic understanding.

Avoid:

> Replaces rust-analyzer, Roslyn, SourceKit, TypeScript, or compilers.

Avoid:

> Always cheaper than Codex or Claude Code.

Avoid:

> CI runs live model benchmarks on every PR.

Avoid:

> All benchmark data is public-ready.

## Source Index

| Source | Used for |
|---|---|
| `docs/internal/VALIDATION_HARNESS.md` | Deterministic harness purpose, runners, replay, planner-probe, live opt-in policy |
| `docs/internal/EVAL_HARNESS.md` | Eval scenarios, mock provider, artifacts, findings, diff/check, cost and token schema |
| `docs/internal/BENCHMARKS.md` | Benchmark layout, CI workflow, public-safe local tables, oracle caveats, full-tier notes |
| `crates/squeezy-eval/src/main.rs` | CLI commands: run, list, replay, view, diff, check |
| `crates/squeezy-eval/src/findings.rs` | Stable auto-finding rule implementation |
| `crates/squeezy-eval/fixtures/scenarios` | Scenario inventory and benchmark scenario families |
| `crates/squeezy-harness/src/main.rs` | Deterministic harness CLI entry point |
| `benchmarks/squeezy-graph-bench/src/report.rs` | Benchmark report schema and public-safe metrics |
| `benchmarks/squeezy-graph-bench/src/gates.rs` | Hard gates and reporting-oriented caveats |
| `benchmarks/corpus.json` | Pinned corpus, tiers, oracle limits, report paths, and caveated entries |
| `benchmarks/specs/*.json` | Smoke query specs and fallback-quality checks |
| `benchmarks/baselines/*.json` | Smoke fixture timing/oracle baselines |
| `docs/internal/eval-findings/*.csv` | Real-world graph-vs-no-graph cost/recall scoreboards |
| `docs/internal/eval-findings/realworld-scoreboard-methodology.md` | Scenario generation, medians, baselines, and grading caveats |
