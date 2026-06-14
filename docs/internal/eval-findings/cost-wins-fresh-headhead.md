# Fresh head-to-head cost-win investigation (perf/cost-wins)

Status: historical investigation log. The committed CSVs now supersede the
intermediate board in this document: `mini-vs-codex-realworld.csv` shows
Mini 15/15 WIN, and `haiku-vs-cc-realworld.csv` shows Haiku 15/15 WIN. Keep
the analysis below for methodology and root-cause history; use the CSVs and
`realworld-scoreboard-methodology.md` for current tallies.

Goal (from the PR): make **squeezy + semantic graph** cheaper than the rivals at
equal correctness on the 15-language realworld eval —
**gpt-5.4-mini vs Codex** (mini tier) and **Claude-Haiku vs Claude Code** (haiku
tier) — and push a CSV showing the result.

This document records the **corrected measurement protocol**, the **eval bugs
that made the prior board untrustworthy**, the **generic squeezy fixes** that
were implemented and validated, and the **honest current board**. It is written
to be reproducible and to state plainly what is solid and what still needs work.

> Historical TL;DR — This pass found stale rival baselines, grader drift, and
> read-heavy tasks where the graph offered little cost advantage. Five
> generic squeezy cost fixes plus three eval-grader bug fixes were implemented.
> Later CSV refreshes improved the board beyond the intermediate counts here.

---

## 1. Measurement protocol (corrected)

Every number here is **fresh, head-to-head, n=3 median, identical pricing and
grader on both sides** — chosen because the committed methodology froze rival
baselines and the cost signal is extremely noisy (per-cell cost spreads of
1.5–2× across rep-sets are routine).

- **Same repo + same question + same grader** for squeezy and the rival. Rival
  prompts are regenerated from the committed scenario `[[steps]]` text (this
  fixed pre-existing java/python/php prompt **drift** — the rival had been
  answering a different question than squeezy on those langs).
- **Pricing is identical per tier** (audited): mini = `$0.75 / $0.075 / $4.50`
  per Mtok (input/cache-read/output) for both squeezy and Codex; haiku =
  `$1.00 / $0.10 / $5.00 / $1.25` (input/cache-read/output/cache-write) for both
  squeezy and Claude Code. CC cost is **recomputed from token usage** (not its
  reported `total_cost_usd`, which runs ~1% high) to match squeezy's own
  token-based accounting.
- **Squeezy cost includes delegate/subagent spend** (`run.json
  totals.cost_micro_usd` = parent + subagents).
- **Rival baselines measured once, n=3, and reused** across squeezy iterations
  (the rival does not change when squeezy changes) — this halves iteration cost
  while keeping the comparison fair.
- **Verdict:** WIN iff `sqz_recall ≥ rival_recall` **and** `sqz_cost ≤ 0.95 ×
  rival_cost` (median). A cell within ±5% is a statistical tie, reported as a
  loss under this strict bar.
- Release binary throughout (debug graph-build is far too slow to fit the
  graph-ready window, which would silently degrade the with-graph arm to grep).

Harness: `/tmp/hth/hth.py` (one lang/tier head-to-head) + `n3.py` (n=k sweep with
rival/squeezy separation) + `board_combined.py` (verdicts). Graders +
ground-truth in `/tmp/codex-runs/realworld/` (see §2).

---

## 2. Eval bugs found and fixed (these corrupted the prior board)

Three grader/ground-truth bugs were making per-language verdicts wrong for
*everyone* (both squeezy and the rivals). All are fixed; each grader fix was
checked to lift **both** sides, not just squeezy.

1. **rust grader scored 0% for everyone.** `ground_truth.json` carried 2-column
   rust sites `[path, line]`, but `grade_rust` matches `site[2]` (the implementor
   *type* name). With no 3rd column, `found` was always 0 — a synthetic perfect
   answer scored **0/15**. Fixed by regenerating the rust ground truth as
   3-column `[path, line, type]` from `new`'s actual production
   `impl LlmProvider for X` sites (15 of them; verified all present in tree). A
   perfect answer now scores 15/15.
2. **c and ts were never scored.** Their ground truth lived only in side files
   (`c_gt.json` 19 rows, `ts_gt.json` 20 rows) and was never wired into
   `ground_truth.json`; the grader returned empty. Merged both in (a good answer
   now scores 19/19 and 20/20).
3. **python grader dropped a correct row.** `grade_python` parsed rows with a
   **line-anchored** regex (`re.match(..., line)`). Models that stream the first
   data row glued to a prose preamble (no leading newline) had that correct row
   silently dropped (e.g. `_ValidatedRequest: (none)`). De-anchored to a
   whole-text `re.finditer` keyed on `.py::Class` — recovers the row with no
   false positives (verified: known-good still 12/12; a real squeezy answer
   10→11/12).

These are squeezy-external graders, but they are the scoreboard's source of
truth, so the fixes are load-bearing for trustworthiness.

---

## 3. Generic squeezy cost/robustness fixes (the product work)

All five are **generic** — they make squeezy cheaper/more-correct for real users,
not benchmark-specific tweaks. Each compiles, is unit-tested, and its effect is
measured below.

### A — Graph-packet slimming (`squeezy-tools/src/graph_tools.rs`)
Graph tool-result packets carried **exact-duplicate** top-level mirrors of data
already in the packet body: a top-level `spans` (== `symbol.path`+`symbol.span`),
a top-level `confidence` (== `symbol.confidence`), and a duplicate `tool` key —
32/32 packets in a measured `definition_search` result. Also emitted empty
relation arrays (`callers:[]`, `callees:[]`, …) and zero-valued span columns.
Dropped the duplicates / omit-when-empty (kept the load-bearing `symbol.id`,
`name`, `kind`, `path`, `signature`, `span`). **20–30% off every graph packet.**
Because tool results re-accumulate in context and are re-billed every provider
round (cache-write at $1.25/Mtok on Anthropic), this compounds across a turn.
Required a one-line consumer fix (`confidence_distribution_from_packets` now
reads body confidence) + inverted snapshot assertions.

### B — `read_slice` resident dedup (`graph_tools.rs`)
`execute_read_slice_blocking` had no resident-read dedup, so the model re-read
byte ranges it already held. Added an **enclosing-window, SHA-gated** dedup
(modeled exactly on the shipped grep resident-dedup): if a prior `read_file`/
`read_slice` snapshot of the **unchanged** file encloses the requested range,
return a receipt stub pointing at it. Recall-safe by construction (only suppresses
a read whose exact bytes are already resident).

### C — Arm mid-turn compaction (`squeezy-agent/src/lib.rs`)
Mid-turn micro-compaction is fully implemented and wired per-round, with
enable-flags defaulting **on** — but it was **dormant**: its gate early-returns
when `context_compaction.model_context_window` is `None`, and that field was
**only ever set from explicit config, never derived from the model registry**.
So in practice compaction *never fired*, and a long single-turn tool storm
re-sent its whole growing transcript every round (≈quadratic). Fixed by
auto-deriving the window from `model_info_for(provider, model).limits
.context_window_tokens` at agent build (explicit config still wins). Only large
single-turn storms (>60% of the window) trigger it, so small tasks are
unaffected. Audited all readers: the window only gates compaction (no hard input
cap), so it cannot truncate context. (Measured: c-haiku ~$0.63→~$0.40 on a
storm rep.)

### D — grep → `decl_search` augment (`squeezy-tools/src/file_ops.rs`)
When the model `grep`s an inheritance-enumeration pattern (`class … with X`,
`extends X`, …), line-oriented grep **misses** declarations whose inheritance
clause wraps onto a continuation line (Dart) or is nested in a large file (Java).
Now such a grep result is **additively augmented** with the matching
declarations from the semantic graph (the same `graph_symbol_search` path
`decl_search` uses, filtering `base:|mixin:|iface:`). Recall-safe by construction
— the model's grep `matches` are never touched; graph decls are sibling fields,
de-duped by `(path, line)`. Degrades to a `reference_search`/`hierarchy` hint for
languages that index inheritance as references (C/C++, JS/TS, Go). **Validated:**
dart-haiku recall **67%→94%** (matching CC's bar), java-haiku **78%→100%**.

### E — Anti-redundant-delegation gate (`squeezy-agent/src/lib.rs`)
On Anthropic, a parent often **both** explores heavily in-context **and** fires a
whole-task `delegate` to a cold subagent that re-reads the same files — double
work (java-haiku: 24 parent calls + 2 delegates, ~$0.32 of which is the subagent
re-deriving the parent's findings). The prior hard `MAX_DELEGATES_PER_TASK=1`
count cap was reverted as eval-specific. This replacement is **principled**:
refuse a whole-task `delegate` only once the parent has *already* gathered
substantial context (`bytes_read ≥ 32KB OR tool_calls ≥ 8`), keyed on the
parent's own exploration magnitude — a context-isolating delegate fired *before*
the parent explores is exempt (both counters near zero). Recall-safe (`Denied`
removes no information; the parent holds the context and keeps every tool;
`Denied` is ignored by the repeated-failure loop guard). **Validated:**
java-haiku **$0.455→$0.267 (WIN 0.72)**. Caveat: where the delegate was *needed*
(parent had to gather more, e.g. c/go-haiku), refusing it shifts the storm
in-context and can cost slightly more — those cells were already losses, so the
gate is net `+1` win with no win lost.

---

## 4. Historical board from this investigation (n=3 medians, all fixes)

Superseded by the checked-in CSVs: Mini is now 15/15 WIN and Haiku is now
15/15 WIN. The tables below are preserved as the intermediate board that
drove the follow-up fixes.

WIN = recall ≥ rival **and** cost ≤ 0.95×. (`*` = coin-flip, ~parity within noise.)

### mini (squeezy gpt-5.4-mini vs Codex gpt-5.4-mini) — historical 11/15 snapshot
| lang | sqz $ | codex $ | ratio | verdict |
|---|---|---|---|---|
| c | 0.0454 | 0.0504 | 0.90 | WIN |
| cpp | 0.0557 | 0.0689 | 0.81 | WIN |
| csharp | 0.0636 | 0.0525 | 1.21 | LOSS (cost) |
| dart | ~0.09 | 0.1802 | ~0.51 | WIN |
| go | 0.0479 | 0.0486 | 0.99 | LOSS* |
| java | 0.1441 | 0.1499 | 0.96 | LOSS* |
| js | 0.0552 | 0.0650 | 0.85 | WIN |
| kotlin | 0.0271 | 0.0416 | 0.65 | WIN |
| php | 0.0261 | 0.0418 | 0.62 | WIN |
| python | 0.0155 | 0.0193 | 0.81 | WIN |
| ruby | 0.0617 | 0.0607 | 1.02 | LOSS* |
| rust | 0.0278 | 0.0355 | 0.78 | WIN |
| scala | 0.0202 | 0.0611 | 0.33 | WIN |
| swift | 0.0134 | 0.0181 | 0.74 | WIN |
| ts | 0.0378 | 0.0424 | 0.89 | WIN |

### haiku (squeezy Claude-Haiku vs Claude Code Haiku) — historical 9/15 snapshot
| lang | sqz $ | cc $ | ratio | verdict |
|---|---|---|---|---|
| c | 0.4416 | 0.2669 | 1.65 | LOSS (cost) |
| cpp | 0.1707 | 0.2074 | 0.82 | WIN |
| csharp | 0.2242 | 0.2364 | 0.95 | WIN |
| dart | ~0.33 (94%) | 0.3754 (94%) | ~0.87 | WIN |
| go | 0.2346 | 0.1575 | 1.49 | LOSS (cost) |
| java | 0.2670 | 0.3696 | 0.72 | WIN |
| js | 0.1644 (68% rec) | 0.0873 (100%) | — | LOSS (recall) |
| kotlin | 0.1159 | 0.2038 | 0.57 | WIN |
| php | 0.1752 | 0.1433 | 1.22 | LOSS (cost) |
| python | 0.2052 | 0.1329 | 1.54 | LOSS (cost) |
| ruby | 0.2178 | 0.2963 | 0.73 | WIN |
| rust | 0.0858 (80% rec) | 0.1509 (73% rec) | 0.57 | WIN |
| scala | 0.1959 | 0.2884 | 0.68 | WIN |
| swift | 0.0215 | 0.0342 | 0.63 | WIN |
| ts | 0.3301 | 0.1807 | 1.83 | LOSS (cost) |

---

## 5. Why the residual cells lose, and the path to 100%

Two findings dominate, and both are *measurement reality*, not squeezy
regressions:

1. **Fresh rivals are markedly cheaper than the committed baselines.** The
   committed mini CSV priced Codex at e.g. cpp $0.106, ruby $0.122, csharp
   $0.087; fresh n=3 Codex is cpp $0.069, ruby $0.061, csharp $0.053. The old
   "15/15" was measured against a more expensive Codex. Beating *today's* cheap,
   heavily-cached Codex is a genuinely higher bar.

2. **The residual losers are read-heavy / grep-shaped enumeration tasks.** c
   (nginx phase-handler audit), csharp (per-subclass override classification),
   php/python/go/ts (per-entity enumeration) require reading code **bodies**;
   the graph finds *where* to read but does not remove the reads, so squeezy
   pays the graph's navigation overhead **plus** the same body reads the
   grep-based rivals do. On these, the graph is close-to-neutral, and against a
   cheap rival that nets a small loss. (`js-haiku` is different: a genuine Haiku
   recall gap on lodash — 68% — that the augment can't help since JS indexes
   inheritance as references, not `base:` attributes.)

**Reaching strict 100% requires option 2 (prompt change) on ~9 of 15 tasks** —
redesigning the read-heavy enumerations into the *cross-file navigation*
questions the graph is actually built for (e.g. resolve-and-enumerate callers of
an overloaded symbol across files, where grep can't disambiguate without reading
many candidates). That is a **substantial benchmark redesign** — and it also
requires **re-baselining the rivals on the new tasks** (the current rival
baselines are for the existing tasks). Because it reframes what the demo claims
and touches most of the suite, it is flagged here for an explicit decision rather
than done silently: the PR author authorized option 2 only "as last resort if the
prompt is genuinely not compatible with the demo," which is exactly this case,
but at this scale it changes the benchmark's character and should be a deliberate
choice.

What is **not** in doubt: the five generic fixes are real cost wins (rust-haiku
0.57, scala-mini 0.33, php-mini 0.62, java-haiku 0.72 via the new delegation
gate, kotlin/swift/cpp across the board) and the three grader fixes make the
board trustworthy for the first time.
