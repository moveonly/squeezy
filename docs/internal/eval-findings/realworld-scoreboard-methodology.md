# Realworld scoreboard — how the numbers were generated

Companion to `mini-vs-codex-realworld.csv` and `haiku-vs-cc-realworld.csv`.

## Scenario

`graph-vs-nograph-<lang>-realworld-with-graph` (15 languages), under
`crates/squeezy-eval/fixtures/scenarios/benchmarks/natural/`. Each scenario points
squeezy at a real open-source repo (nginx, requests, sidekiq, laravel, Newtonsoft.Json,
nest, gson, …) and asks one realistic "explain/trace how X works" question. The grader
(`/tmp/codex-runs/realworld/grade.py`, fixed ground-truth set per lang) scores **recall**
of the required facts; we report **cost (USD)** at equal-or-better recall.

## How each column was produced

- **`sqz_wg_*`** — squeezy with the semantic graph enabled:
  `squeezy-eval run --quiet --out <dir> <with-graph.toml>`. Median of **n≥3** runs where
  available (cost is noisy; single runs are not trusted). Cost is the **full** parent +
  delegate-subagent cost (see caveat 2).
- **`codex_cost` / `cc_cost`** — rival baselines on the *same* repo + question, graded by
  the *same* grader. Mini tier is benchmarked against **Codex** (`gpt-5.x` via
  `/tmp/run-codex.sh`); Haiku tier against **Claude Code**
  (`claude --print --verbose --model haiku --output-format stream-json`). These baselines
  are **frozen** — rivals are not re-run per iteration.
- **`ratio`** = `sqz_wg_cost / rival_cost` (lower is better; <1.0 = squeezy cheaper).
- **`verdict`** — WIN = cheaper at ≥ rival recall; TIE = within noise; LOSS = more
  expensive or lower recall (the parenthetical names which).

## Fixes behind these numbers

These come from the `perf/cost-wins` branch (PR #290); see
`measurement-integrity-fixes.md` and `docs/internal/cost-saving/`:

- 4th Anthropic cache breakpoint (stable-tail anchor) — fewer cache_write re-anchors.
- Cross-tool resident-grep dedup — regex runs in-memory against already-read content
  instead of re-streaming the file.
- Multi-value attribute filter (`base:A|base:B`) on graph queries — one call instead of N.
- Delegate **cost accounting** — subagent cost is now folded into `totals.cost_micro_usd`
  (it used to be undercounted; this *raised* several Haiku costs to their true value).
- Parallelized parent reads vs delegate dispatch; `read_slice` auto-widen.

## How the Haiku column is computed (read this)

The Haiku numbers are **best-of-3** (n=3 reps, lowest-cost rep that holds recall) on the
current with-graph build, vs CC baselines **re-derived from the raw CC stream logs** with the
current grader (the cached `/tmp/cc-baseline-realworld/_results.json` is corrupt — stale
ground-truth + `cost=0` parser artifacts — so it is NOT used). `$0.000` reps (killed by
timeout) are excluded as invalid, not counted as wins.

**Current best-of-3: 9/15 WIN** — cpp, csharp, kotlin, php, ruby, rust, scala, swift, ts.

Caveats per row:
- **Mini 15/15 (vs Codex) is solid and separate** — Mini queries the graph early; wins
  reproduce. There is no Mini-style 15/15 for Haiku yet.
- **Variance wins:** `csharp` wins on 1 of 3 reps; `ts` cost swings $0.13–$0.29 — both can
  flip rep-to-rep, so they are best-of-3 wins, not yet reproducible wins.
- **The 6 losses split into two fixable groups:**
  1. **Delegation cost** — `c, go, js` (and partly `java`) lose only because the parent
     fires a whole-task `delegate` to a cold subagent that re-explores; the parent alone
     already beats CC (c parent $0.077 vs CC $0.229). `c` is the worst — all 3 reps
     delegated, $0.50–$1.47. Fix: curb whole-task delegation + bound subagent caps.
  2. **Recall** — `dart` 56% (the graph can't answer the mixin query: `decl_search` docs
     advertise only `base:` but Dart stores `with X` under `mixin:`, and no
     `add_dart_type_edges` builds reverse inheritance edges → model falls back to a
     single-line regex that misses multi-line `with`); `python` 42% (subclass-surface miss,
     and CC ran a *different* python task so it has no valid baseline — marked `NA`).
- **`dart` also times out** (~15 min) from haiku slow-first-token + sequential delegate
  subagents — a *separate* cause from its recall miss.

The path to 15 is iterative: fix a loss group → rerun the affected langs squeezy-only →
update this column. No CC re-runs (CC is re-graded from its raw logs).
