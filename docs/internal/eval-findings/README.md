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
| `cc_cost` / `codex_cost` | median USD cost of the external baseline (`cc_cost` in the Haiku CSV, `codex_cost` in the Mini CSV) on the same scenario |
| `ratio` | `sqz_wg_cost / baseline_cost` (1.00 = parity, <1.00 = cheaper than baseline) |
| `verdict` | **WIN** iff `sqz_wg_recall ≥ baseline recall` AND `ratio ≤ 0.95`; **LOSS** otherwise |

## Scenarios

15 language scenarios under
[`crates/squeezy-eval/fixtures/scenarios/benchmarks/natural/graph-vs-nograph-{lang}-realworld-{with,no}-graph.toml`](../../../crates/squeezy-eval/fixtures/scenarios/benchmarks/natural).
Each is a multi-step audit on a real upstream OSS repo pinned by SHA
(e.g. `google/gson` for java, `akka/akka` for scala, `flutter/flutter`
for dart, `nginx/nginx` for c). The `[with,no]-graph` pair runs the
identical prompt with the 12 graph tools available or
hidden via `[tools] excluded` overlay.

## Sweep mechanics

The reproducible scripts for this board are vendored under
[`realworld-harness/`](realworld-harness/). The `/tmp/...` paths below are the
historical scratch locations used by the captured sweep; regenerate from the
vendored harness when repeating the measurement.

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
- **Codex CLI (gpt-5.4-mini)**: `codex exec --model gpt-5.4-mini ...`. Per-run results in `/tmp/codex-runs/realworld/results.json`.

Baselines were captured against the same scenarios and treated as fixed
references for the sweep. They use the same n=3 reps with the same
grader.

## Grading

- Per-language graders in `realworld-harness/grade.py` (historically copied to
  `/tmp/codex-runs/realworld/grade.py`) score each model answer against ground
  truth captured in each scenario's `description` block. Recall = found / total
  expected rows.
- Ground truth lives in `realworld-harness/ground_truth.json`.

## CSV generation

`realworld-harness/board_combined.py` walks the fresh sweep run directories, medians
cost (dropping `$0` failures) and recall (dropping zero-total grader misses) per
cell, joins baselines, classifies, and writes the two CSVs.

```
python3 docs/internal/eval-findings/realworld-harness/board_combined.py
```

## Snapshot tally (committed CSVs)

| | Haiku w/g vs CC | Mini w/g vs Codex |
|---|---|---|
| WIN | 15 | 15 |
| LOSS | 0 | 0 |

These counts come from the checked-in `haiku-vs-cc-realworld.csv` and
`mini-vs-codex-realworld.csv`. Treat older runbooks in this directory as
historical if their prose disagrees with those CSVs.
