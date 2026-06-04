# Realworld head-to-head harness (vendored for reproducibility)

These are the corrected grader + harness behind
[`../cost-wins-fresh-headhead.md`](../cost-wins-fresh-headhead.md). They were
previously scattered in `/tmp`; vendored here so the board is reproducible and
the grader bug-fixes are reviewable.

- **`grade.py`** — per-language recall graders. Fixes vs the prior version:
  rust now matches the implementor *type* (3-column ground truth, not a
  line-number that drifts), and `grade_python` is de-anchored so a correct row
  glued to a prose preamble is still counted.
- **`ground_truth.json`** — ground truth for all 15 langs. Fixes: rust is now
  3-column `[path, line, type]` (was 2-column → scored 0% for everyone); `c`
  (19 rows) and `ts` (20 rows) are now wired in (were never scored).
- **`hth.py`** — one lang × tier head-to-head: runs squeezy + the rival on the
  same repo/question, grades both with the same grader, identical per-tier
  pricing (CC cost recomputed from token usage to match squeezy's accounting).
- **`n3.py`** — n=k sweep with rival/squeezy separation (rival baselines measured
  once and reused across squeezy iterations).
- **`board_combined.py`** — verdicts (WIN iff recall ≥ rival **and**
  cost ≤ 0.95×) → the committed `*-realworld.csv` files.
- **`analyze.py`** — per-run cost-component breakdown (raw-input / cache-read /
  cache-write / output, tool-call counts, findings) used to diagnose each loss.
- **`gen_inputs.py`** — regenerates rival prompts + Haiku tomls from the committed
  scenarios (eliminates prompt drift between squeezy and the rival).

Rival CLIs: `codex exec -m gpt-5.4-mini` (mini) and
`claude --print --model haiku --output-format stream-json` (haiku).
