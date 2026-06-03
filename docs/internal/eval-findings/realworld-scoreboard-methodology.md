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

## Caveats (read before trusting the Haiku column)

1. **Mini 15/15 (vs Codex) is solid.** Mini queries the graph early, so the graph attaches
   reliably; the wins reproduce across runs. This is the *only* 15/15 board — there is no
   Haiku-vs-CC 15/15 and never was (the four haiku-vs-cc scoreboards on disk peak at 10).
2. **Haiku is currently 5/15 (WIN: cpp, php, rust, scala, swift), cost-driven not recall.**
   Computed from the newest `run.json` (full parent+subagent cost) vs the frozen CC
   baselines in `/tmp/cc-baseline-realworld/_results.json` (n=3 medians). Recall is at
   parity everywhere (only rust 93.8%, and it still wins). Two cost effects, both being
   worked: (a) **delegate-to-subagent** — 6 langs (c, dart, go, java, js, python) issue a
   single `delegate` that hands the whole task to a same-model subagent which re-explores
   from scratch, so subagent spend dominates (e.g. c = $0.077 parent + $0.260 subagent =
   $0.338), and `15c226d8` now correctly counts it; (b) **parent round-trips** — csharp,
   ts, scala, ruby are parent-only yet expensive from per-method `read_slice`/grep chains.
   There is **no delegate-storm** (each delegating lang calls `delegate` exactly once).
3. **The graph builds eagerly before the first tool call** (confirmed in code): `graph_used`
   is false for some langs only because Haiku never *queries* the graph (audit-style
   prompts don't trigger the exploration preflight, and Haiku acts before querying), not
   because the graph failed to load. The fix is model-steering (preflight for audit intents
   / delegate gating), not graph startup.
