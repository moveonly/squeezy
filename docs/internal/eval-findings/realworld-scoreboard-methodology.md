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

## Current CSV snapshot

The checked-in CSVs are the current source of truth for this directory:

- `mini-vs-codex-realworld.csv`: 15/15 WIN.
- `haiku-vs-cc-realworld.csv`: 15/15 WIN.

The Haiku numbers are computed against CC baselines derived from raw CC stream
logs with the current grader. `$0.000` timeout reps are excluded as invalid,
not counted as wins.
