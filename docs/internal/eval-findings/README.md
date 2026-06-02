# Realworld graph-vs-nograph eval — how the CSVs are produced

Two narrow comparison CSVs next to this file:

- `haiku-vs-cc-realworld.csv` — squeezy Haiku 4.5 (with-graph + no-graph) vs Claude Code `--bare` (Haiku tier).
- `mini-vs-codex-realworld.csv` — squeezy gpt-5.4-mini (with-graph + no-graph) vs Codex CLI (gpt-5.4-mini tier).

Each row is one language. Columns:

| column | meaning |
|---|---|
| `lang` | language under test |
| `sqz_wg_recall` | median recall % across with-graph reps (graded vs ground-truth) |
| `sqz_wg_cost` | median USD cost across with-graph reps |
| `baseline_cost` | median USD cost of the external baseline (CC or Codex) on the same scenario |
| `ratio` | `sqz_wg_cost / baseline_cost` (1.00 = parity, <1.00 = cheaper than baseline) |
| `sqz_ng_recall` / `sqz_ng_cost` | same, for the no-graph half of squeezy |
| `verdict` | **WIN** iff `sqz_wg_recall ≥ 95%` AND `sqz_wg_cost ≤ 0.95 × baseline_cost`; **TIE** if recall passes but cost within ±5% of baseline; **LOSS** otherwise; **N/A** if no baseline data |

## Scenarios

15 language scenarios under
[`crates/squeezy-eval/fixtures/scenarios/benchmarks/natural/graph-vs-nograph-{lang}-realworld-{with,no}-graph.toml`](../../../crates/squeezy-eval/fixtures/scenarios/benchmarks/natural).
Each is a multi-step audit on a real upstream OSS repo pinned by SHA
(e.g. `google/gson` for java, `akka/akka` for scala, `flutter/flutter`
for dart, `nginx/nginx` for c). The `[with,no]-graph` pair runs the
identical prompt with the 12 graph tools available or
hidden via `[tools] excluded` overlay.

## Sweep mechanics

Per cell (lang × variant × model), n = 3 reps. Each rep is a fresh
ephemeral workspace clone + a single agent turn capped at 600s (1500s for
dart-on-Flutter SDK because the workspace is huge).

Squeezy side, parallel launcher (15 langs concurrent):

```
for lang in c cpp csharp dart go java js kotlin php python ruby rust scala swift ts; do
  bash /tmp/full-sweep/par_validate.sh "$tier:$lang" &
done
# tier in {haiku, mini}; one batch fully completes before the other launches.
```

`par_validate.sh` runs:

```
timeout -k 60 $CAP target/release/squeezy-eval run --quiet --out target/eval "$toml"
```

for each `(variant ∈ {with-graph, no-graph}) × (rep ∈ {1,2,3})`.

## Baselines

- **Claude Code (Haiku)**: `claude --print --model claude-haiku-4-5-20251001 --bare ...` with the same prompt against the same workspace. Per-run results in `/tmp/cc-baseline-realworld/_results.json`.
- **Codex CLI (gpt-5.4-mini)**: `codex exec --model gpt-5.4-mini ...`. Per-run results in `/tmp/codex-runs/realworld/results.json`. C + TS not in baseline → `N/A` verdict.

Baselines were captured against the same scenarios and treated as fixed
references for the sweep. They use the same n=3 reps with the same
grader.

## Grading

- Per-language graders in `/tmp/codex-runs/realworld/grade.py` (one
  `grade_{lang}` per language) score each model answer against ground
  truth captured in each scenario's `description` block. Recall =
  found / total expected rows.
- Ground truth lives in `/tmp/codex-runs/realworld/ground_truth.json`.

## CSV generation

`/tmp/full-sweep/build_csv.py` walks the fresh sweep run directories,
medians cost (dropping `$0` failures) and recall (dropping
zero-total grader misses) per cell, joins baselines, classifies, and
writes the two CSVs.

```
python3 /tmp/full-sweep/build_csv.py
```

## Snapshot tally (this PR's HEAD)

| | Haiku w/g vs CC | Mini w/g vs Codex |
|---|---|---|
| WIN | 7 | 6 |
| TIE | 2 | 2 |
| LOSS | 6 | 5 |
| N/A | 0 | 2 (c, ts) |

`N/A` cells need a codex baseline run before they can be classified.
The remaining w/g LOSSes group into three mechanism families (graph
packet wire weight, runaway grep when the model bypasses graph,
Haiku-delegate batch-read) — left for follow-up PRs.
