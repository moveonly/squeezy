# Plan: an UNBIASED demonstration of squeezy's semantic-graph value

Status: historical execution plan. The repo-local current scoreboard lives in
`mini-vs-codex-realworld.csv`, `haiku-vs-cc-realworld.csv`, and
`realworld-scoreboard-methodology.md`; older branch names, `/tmp/hth` paths,
and intermediate win counts below are retained as execution context rather
than current instructions.

> Self-contained execution plan. An agent with **no prior context** should be
> able to pick this up and execute it. Read §1–§4 for the goal, principle, and
> environment; §5 is the step-by-step work; §6 is the reference/runbook.

---

## 1. Goal

Demonstrate, **without bias**, that squeezy + its semantic graph delivers real
value: **lower cost at equal-or-better correctness** vs the rivals, across the 15
benchmark languages, on tasks that **genuinely need** cross-file semantic
resolution. Produce two clean demo boards (comprehension + code-gen) at **100%
wins**, plus the squeezy/graph improvements and methodology that make the result
honest and reproducible.

- Mini tier: squeezy on OpenAI `gpt-5.4-mini` **vs `codex exec -m gpt-5.4-mini`**.
- Haiku tier: squeezy on Anthropic Claude-Haiku **vs Claude Code (`claude --print --model haiku`)**.

## 2. The anti-bias principle (READ THIS — it governs every task choice)

We do **not** pick tasks to make squeezy win. We pick tasks where the **graph's
value is real**, and we measure honestly. Two hard rules:

1. **A task qualifies only if it genuinely needs the graph.** Operationally: in
   the existing `graph-vs-no-graph` harness, squeezy's **no-graph arm
   (grep+read)** must be **meaningfully more expensive** than its **with-graph
   arm** on the same task (suggested bar: `no_graph_cost ≥ 1.20 × with_graph_cost`
   at equal recall, n=3 median). If the no-graph arm is as cheap, the task does
   not need the graph and is a **bad demo** — replace it with a real task that
   does (never with a "compute the graph" prompt — see rule 2).
2. **Never ask the agent to compute what the graph computes.** Prompts like
   "list all callers of X" or "enumerate the call graph" are biased: they ask the
   agent to *produce the graph*, which trivially favors squeezy. Tasks must be
   **real developer work** — a question a developer asks to understand/modify the
   code, or an actual edit — where the graph is *instrumentally* useful but the
   deliverable is an answer/edit, graded on its correctness.

Where squeezy **loses** on a genuinely-graph-needing task, root-cause it:
- **(a) task-fit:** the task is grep-shaped (graph adds no value) → replace it
  with a genuine graph-needing task (per rules 1–2);
- **(b) squeezy/graph limitation:** the task needs the graph but squeezy uses it
  inefficiently or the graph can't answer it → **fix squeezy/graph generically**
  (a real product improvement, not a benchmark hack).

## 3. Current state (what already exists — do not redo)

**Merged:** PR #290 (`origin/main` @ `c7fb8bf`) landed:
- 5 **generic** squeezy fixes (all unit-tested): graph-packet slimming, read_slice
  resident dedup, **armed mid-turn compaction** (auto-derive `model_context_window`
  from the registry), **grep→decl_search augment** (additive, recall-safe), and an
  **anti-redundant-delegation gate** (refuse a whole-task delegate after the parent
  already explored ≥8 calls / ≥32KB). See `cost-wins-fresh-headhead.md`.
- 3 **eval-grader bug fixes**: rust GT was 2-column (scored 0% for everyone), c/ts
  GT were never wired in, python grader's row regex was line-anchored. All fixed.
- The corrected **honest board** (fresh head-to-head, n=3 medians, identical
  pricing/graders): `mini-vs-codex-realworld.csv`, `haiku-vs-cc-realworld.csv`.
- Vendored harness + graders under `docs/internal/eval-findings/realworld-harness/`.

**Starting board:** the loss lists in this plan are work-queue notes, not the
current scoreboard. The checked-in CSVs show **mini 15/15 WIN** and **haiku
15/15 WIN**.

**Why the residual losses happen (already investigated):** the residual-loss
tasks are **read-heavy enumerations** ("list every X under dir Y") where grep+read
is as cheap as the graph, so the graph adds overhead and squeezy doesn't win
against today's (much cheaper than the old committed baseline) rivals. These are
exactly the **task-fit (a)** failures from §2 — bad graph demos to be replaced.

**Code-gen pilot already run (key finding):** a real refactor — *rename
`Session.send`→`dispatch` in psf/requests, updating only Session-receiver call
sites and leaving `sock.send` alone* — showed squeezy **cheaper ($0.070 vs codex
$0.093) but grossly under-thorough**: it edited ~4 of ~21 required sites (skipped
the typing-overload stub and ALL ~17 test call sites). codex edited ~all. So
**squeezy is cheaper only because it does less** → a fair recall-of-edits grade is
a LOSS. **The real product gap: squeezy does not exhaustively find+edit every
cross-file site (the graph's `upstream_flow` can give them all).** This is the
Step-4 squeezy improvement.

## 4. Environment & harness (how measurement works)

- **Historical repo path:** `/Users/example/esqueezy/new`. Work on a branch off `main`
  (current scratch branch `perf/graph-favorable-tasks` — rebase onto `main` or
  start fresh; it currently has nothing but this plan).
- **Release binary** (rebuild after any squeezy code change):
  `cargo build --release -p squeezy-eval` → `target/release/squeezy-eval`.
  (Always use **release**; debug graph-build is too slow and silently degrades the
  with-graph arm to grep.)
- **Scenarios:** `crates/squeezy-eval/fixtures/scenarios/benchmarks/natural/graph-vs-nograph-<lang>-realworld-{with,no}-graph.toml`.
  Each has `[workspace.github] repo + sha` (rust uses `[workspace] local="."`),
  `[squeezy] provider/model/mode/instructions`, and the `[[steps]] text` (the
  question). `mode = "build"` + `permission_mode = "allow"` already allow edits.
- **Run squeezy on a scenario:**
  `target/release/squeezy-eval run --quiet --out <dir> <scenario.toml>`
  → writes `<dir>/<id>-<ts>/{run.json, frames.jsonl, trace.jsonl}`. Cost =
  `run.json totals.cost_micro_usd` (parent + subagents). Final answer = last
  `frames.jsonl` line's `assistant_text`. For **in-place editing** (code-gen),
  add `--workspace-override <local-clone>` — squeezy edits that dir directly
  (`snapshot=false`), so `git -C <clone> diff` captures the edits.
- **Haiku tier:** same scenario with `[squeezy] provider="anthropic", model="claude-haiku-4-5-20251001"`.
  Generate via `/tmp/hth/gen_inputs.py` (swaps provider/model, writes
  `/tmp/hth/haiku-toml/<lang>-{with,no}-graph.toml` + the rival prompt files).
- **Rivals (same repo+question, identical pricing):**
  - mini: `codex exec --json --ignore-user-config --ephemeral --skip-git-repo-check -C <repo> -m gpt-5.4-mini "<prompt>"`. Cost priced `$0.75/$0.075/$4.50` per Mtok (in/cache/out) from `turn.completed.usage`. For editing add `--dangerously-bypass-approvals-and-sandbox`.
  - haiku: `claude --print --model haiku --output-format stream-json --verbose --bare --permission-mode bypassPermissions --tools Read Grep Glob Bash < <prompt>` (scrub `CLAUDE_CODE_*`/`CLAUDECODE` env). Cost **recomputed from token usage** at `$1.00/$0.10/$5.00/$1.25` (in/cache-read/cache-write/out) — NOT the reported `total_cost_usd` (~1% high) — to match squeezy's token-based accounting.
- **Harness scripts** (`/tmp/hth/`, also vendored at `docs/internal/eval-findings/realworld-harness/`):
  `hth.py <lang> <mini|haiku> <squeezy|rival|both> <n> [with-graph|no-graph]` (one cell head-to-head),
  `n3.py` (n=k sweep, rival/squeezy separated; **note bug**: same `--label` truncates the output file — use distinct labels),
  `board_combined.py` (verdicts → CSV), `analyze.py <rundir>` (per-run cost breakdown: raw-in/cache-read/cache-write/output, tool counts, findings).
- **Graders:** `/tmp/codex-runs/realworld/grade.py` (`GRADERS[lang]`, `grade_<lang>`) + `ground_truth.json`. Verdict = **WIN iff `sqz_recall ≥ rival_recall` AND `sqz_cost ≤ 0.95 × rival_cost`** (n=3 median). Pricing audited identical per tier.
- **Cloned repos** for rivals at `/tmp/hth/repos/<lang>` (c,cpp,csharp,dart,go,java,js,kotlin,php,python,ruby,scala,swift,ts). Repo+sha map: nginx/nginx, gabime/spdlog, JamesNK/Newtonsoft.Json, flutter/flutter, spf13/cobra, google/gson, lodash/lodash, detekt/detekt, laravel/framework, psf/requests, sidekiq/sidekiq, akka/akka, vapor/vapor, nestjs/nest (rust = the squeezy repo itself, `local="."`).
- **Keys:** `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` in env (`source $HOME/.env.sh` for the OpenAI key). Anthropic throttles under load — pace haiku runs (lc≤2). The eval is **high-variance** (per-cell cost spreads 1.5–2× across rep-sets) → always use **n=3 medians**; escalate close calls.

---

## 5. The plan (execute in order)

### Track A — COMPREHENSION demo (do this first)

**Step A1 — Root-cause every residual loss.**
For each losing lang (`c, csharp, dart, go, java, js, php, python, ruby, ts`),
classify the *current* task with evidence:
- Measure squeezy **with-graph vs no-graph** (n=3) on the current task:
  `hth.py <lang> <tier> squeezy 3 with-graph` and `... no-graph`.
- **If `no_graph ≤ 1.2 × with_graph`** → the task does NOT need the graph (grep
  works) → **task-fit failure → replace the task** in Step A2.
- **If `no_graph ≥ 1.2 × with_graph` but squeezy still loses to the rival** → a
  **squeezy/graph limitation** → record it for Step A3 (with `analyze.py` evidence:
  which tool calls / tokens dominate, is the graph available, did it over-explore).
- Output: a table `lang | with_graph | no_graph | ratio | rival | verdict | class (replace|fix) | evidence`.
- This is parallelizable: one investigation agent per lang.

**Step A2 — Build the comprehension demo (genuine graph-needing questions).**
For every lang, ensure the task is a **real question that genuinely needs the
graph** and is **not** "compute the graph":
- Keep tasks that already pass the §2 no-graph check and win.
- For the "replace" langs, design a real comprehension question whose **correct
  answer requires cross-file semantic resolution** grep can't cheaply do. Good
  shapes (the deliverable is an *answer*, graded by recall of required facts):
  - "When `<entrypoint>` handles `<input>`, which `<category>` functions run
    (across files), in order?" (call-chain tracing).
  - "If I change `<symbol>`'s `<aspect>`, which call sites would break and why?"
    (impact, requiring receiver/override resolution — but graded on the *reasoning
    facts*, not a raw caller dump).
  - "Which concrete `<impl of T>` is selected at `<call site>` given the type
    hierarchy, and what does it do?" (overload/override resolution).
  Pick a hub/site where grep over-matches (common name, overloads, inheritance) so
  the no-graph arm must read many files — then **validate** the no-graph-arm check
  (rule 1) holds before keeping the task.
- Write a small, **format-agnostic recall grader** + a **verified** ground truth
  (derive it independently — tree-sitter / careful tooling — and double-check; a
  wrong GT poisons the benchmark). Reuse `grade_*` patterns in `grade.py`.
- Regenerate rival prompts + haiku tomls (`gen_inputs.py`), **re-baseline both
  rivals** on the new task (their old baselines are for the old task), re-measure
  squeezy, compute verdicts (`board_combined.py`).
- **Done when:** all 15 langs WIN on both tiers (recall parity, ≤0.95× cost) **and
  each win passes the no-graph-arm check** (the graph is provably doing the work).
- Parallelizable: one design agent per "replace" lang (then human/lead verifies GT
  + the no-graph check before measuring).

**Step A3 — Fix the real squeezy/graph limitations found in A1.**
For langs classified "fix": implement **generic** squeezy/graph improvements (no
benchmark-specific hacks). Candidates seen so far: redundant graph re-queries,
over-reading whole files where slices suffice, the augment only firing on grep
(extend to the patterns the model actually uses), graph availability/latency on
large repos. Rebuild, re-measure, confirm the win is graph-attributable.

### Track B — CODE-GEN demo (after Track A is solid)

**Step B1 — Squeezy improvement: graph-driven exhaustive code editing.**
The pilot proved squeezy is **under-thorough** on cross-file refactors. Make it a
real product win:
- When the agent performs a refactor of a symbol, it should use the graph
  (`upstream_flow`/`reference_search`/`hierarchy`) to enumerate **every** affected
  site (incl. tests/docs) and **edit them all**, resolving ambiguous receivers
  (e.g. `s.send` where `s:Session` vs `s:socket`) via the graph rather than
  grepping. Investigate whether this needs: (i) a behavior/steer so the agent
  closes the loop "find all refs → edit each", (ii) **new/extended editing tooling**
  (e.g. a graph-aware "rename symbol" or "apply edit at all resolved references"
  operation that's cheaper and more reliable than N manual edits — this is the
  "add some editing stuff" item), and/or (iii) a completeness self-check before
  finishing. Keep it generic (helps real users doing refactors).
- Acceptance: on a held-out refactor, squeezy reaches **full recall of required
  edits** (def + every true call site, none of the false ones) at **lower cost**
  than the rival.

**Step B2 — Build the code-gen benchmark (real refactors, strict correctness).**
- **Diff-capture harness:** per rep, fresh-clone the repo to a temp dir, run
  squeezy with `--workspace-override <clone>` (edits in place) / run the rival
  `cd <clone>` (codex with `--dangerously-bypass-approvals-and-sandbox`; claude
  with `--permission-mode bypassPermissions`), then `git -C <clone> diff` →
  grade the diff. Use a separate clone per rep (concurrent reps must not share).
- **Task shape:** a real refactor needing cross-file resolution — rename/migrate
  an **ambiguously-named** symbol (grep over-matches; the graph resolves the
  receiver/type). The `Session.send` vs `sock.send` pilot is the template.
- **Prompt:** a **solid, unambiguous prompt with an explicit correctness contract**
  — state exactly what must change and what must NOT (e.g. "rename `T.M`→`N`: the
  definition(s) and every call whose receiver is a `T`; do NOT touch `M` on other
  types; keep it compiling"). Ambiguity in the prompt makes grading unfair.
- **Grading = recall of the required edits at the exact sites** (so partial work
  can't fake a cheap win), plus a **precision** check (didn't edit the
  must-not-touch sites), and ideally a **compile/parse check** (the edited tree
  still parses). Define the GT as the exact `(file, site)` set that must change
  and the set that must not. **Verify the GT independently** (tree-sitter / type
  resolution). Both agents graded identically.
- **Validate it needs the graph** (rule 1): squeezy no-graph arm should pay much
  more (read every `.M(` site to disambiguate) than with-graph.
- Re-baseline rivals on each refactor, measure squeezy (n=3, both tiers),
  verdicts. **Done when:** all langs WIN (full recall at lower cost).

### Step 5 — Finalize
- Two CSV boards: `comprehension-*.csv` and `codegen-*.csv` (mini + haiku each).
- Update `methodology` MD: the anti-bias principle (§2) + the no-graph-arm
  validation + per-lang task rationale (why each genuinely needs the graph) + the
  squeezy/graph improvements (A3, B1) + the code-gen grading contract.
- Vendor the final graders + GT + new scenarios under `realworld-harness/`.
- Commit the squeezy/graph code changes + scenarios + GT + CSVs + methodology and
  open a **new PR** (rebased on `main`). Ensure CI green (`cargo fmt --all` + clippy
  + tests before pushing — the "Validate and debug artifact" job runs all three).

---

## 6. Reference / runbook

- **One cell, head-to-head, n=3:** `MAXW=2 python3 /tmp/hth/hth.py <lang> <mini|haiku> both 3` → prints `SUMMARY {...verdict...}`.
- **Cost breakdown of a run:** `python3 /tmp/hth/analyze.py <rundir>` (shows raw-in/cache-read/cache-write/output split, tool-call counts, the eval's own findings like `high_tool_burst`, `deep_chain_expansion`).
- **No-graph-arm check (the anti-bias gate):** compare `hth.py <lang> <tier> squeezy 3 no-graph` vs `... with-graph`; require no-graph ≥ ~1.2× with-graph.
- **Re-baseline a rival on a changed task:** the rival baselines in `results-rival-<tier>.jsonl` are per-task — **rerun the rival whenever the task changes** (the prompt is regenerated by `gen_inputs.py` from the scenario `[[steps]]`).
- **Pricing (identical per tier, audited):** mini `$0.75/$0.075/$4.50`; haiku `$1.00/$0.10/$5.00/$1.25` (in/cache-read[/cache-write]/out per Mtok). squeezy cost from `run.json`; codex from `turn.completed.usage`; CC recomputed from `modelUsage`/`usage` token counts.
- **Variance:** n=3 medians minimum; the harness retries `$0` (transient stream/throttle) runs. dart/flutter is very slow (use the 1500s cap; consider fewer reps).
- **Branch hygiene:** rebase onto `main` before the new PR; run `cargo fmt --all` + `cargo clippy -p <changed> --all-targets` + tests locally (the prior PR went red purely on `cargo fmt --check` of agent-written code).

## 7. Status checklist (update as you go)
- [x] A1 root-cause table (all 10 losing langs classified replace|fix, with no-graph deltas) — see §8
- [ ] A2 comprehension tasks designed + GT verified + no-graph-check passed (per lang)
- [ ] A2 comprehension board = 15/15 both tiers, every win graph-attributable
- [~] A3 squeezy/graph limitation fixes (generic) landed + re-measured — GAP 2 landed; GAP 1 in progress (§8)
- [ ] B1 squeezy graph-driven exhaustive-edit improvement (+ any new editing tool)
- [ ] B2 diff-capture + recall/precision grader harness
- [ ] B2 code-gen tasks + verified GT + no-graph-check + board = wins both tiers
- [ ] 5 CSVs + methodology + vendored harness + new PR (CI green)

---

## 8. Execution state & findings (live — updated 2026-06-04)

### Reframed goal (from the lead)
The graph's value is **composability** — replacing N grep/read calls with one higher-level
query (fewer tool calls), even on grep-friendly repos. Priority: make squeezy genuinely better
(real graph/tool improvements). Iterate task/repo only as a fallback. Anti-bias (§2) still governs.

### A1 results (clean binary, n=3 medians; ratio = no-graph ÷ with-graph cost)
- **REPLACE (7)** — grep-shaped, graph adds no discovery value (precise greppable membership
  marker): `go, python, js, csharp, c, php, dart`. (go/python repos are also too small: 14/20 files.)
- **FIX (3)** — graph genuinely helps (ratio ≥ 1.2): `java 1.77, ts 1.27, ruby 1.22`. But on the
  clean binary squeezy still LOSES/TIES the rival head-to-head: **java LOSS 1.10** (high variance),
  **ts LOSS 1.26**, **ruby TIE 0.99**. => not yet graph-attributable WINS; A3 is the path.
- Mechanism: the graph wins only when the membership marker is grep-**noisy** (java `extends
  TypeAdapter` over-matches → no-graph does 15 greps + 15 whole-file reads = 677k tok; with-graph
  1 decl_search + tight slices = 178k). Precise markers (`use Macroable`, `with Mixin`, `:
  JsonReader`) → grep is as cheap → REPLACE. NOTE `decl_search base:` is DIRECT-only (not transitive).
- Detail: `.work/a1/A1-FINDINGS.md`, `.work/a1/A1-table-final.json`.

### Measurement-integrity fixes landed (generic, committable)
- `crates/squeezy-eval/src/driver.rs` `timestamp_dir_slug`: run dir `{slug}-{ms}` collided under
  concurrency → clobbered run.json → corrupted cost. Now `{slug}-{ms}-{pid}-{seq}`. (Flipped
  go-haiku's phantom 1.30→0.89.)
- Reset the live `/tmp` graders+GT to the committed/vendored baseline (discarded a half-finished,
  anti-bias-violating "caller-enumeration" c/java GT/grader drift). Re-derive every new GT
  independently (a scout undercounted laravel Macroable 38 vs the true 75).

### A3 — generic squeezy improvements (the real work; .work/A3-ROADMAP.md)
The model grep+read_file-storms on FIX langs because the graph can't give it a cheap subclass
enumeration. Prioritized:
- **GAP 1 (headline, LANDED for JS/TS):** JS/TS recorded `extends`/`implements` only as
  type-reference edges, never as queryable `base:`/`iface:` ATTRIBUTES → `decl_search base:X` and
  the grep→graph augment returned nothing for TS/JS → model fell back to grep + 25 whole-file
  reads (ts). FIX (`crates/squeezy-parse/src/languages/rust.rs` `js_ts_attributes_for_symbol` +
  new `js_ts_heritage_split`/`strip_angle_groups`): class/interface symbols now carry
  `base:<extends>` / `iface:<implements>`, robust to generic constraints (`<T extends X>`),
  generic bases (`Base<T>`→`Base`), member-expression bases (`ns.Core`→`Core`), and multi-interface.
  Validated by deterministic tests: squeezy-parse `js_ts_class_heritage_emits_base_and_iface_attributes`
  + squeezy-graph `graph_records_js_ts_class_heritage_as_base_and_iface_attributes` (the build
  carries attributes through; decl_search/augment match attributes language-agnostically). The
  attribute alone is sufficient for decl_search/augment (no edge builder needed; edges are only for
  inherited-method call resolution). TODO: same for C (`c_family.rs`) and Go (`go.rs`) if pursued.
- **GAP 2 (LANDED):** grep→graph augment dropped all-but-last supertype in `extends (A|B|C)`. Now
  enumerates all (`file_ops.rs` `InheritanceGrep.base_name` → `base_names: Vec`). Unit-tested.
- **GAP 3 (LANDED):** Ruby DID index `include` but as `mixin:include:<T>`, which the augment's
  `mixin:<T>` query can't substring-match. Now emits BOTH `mixin:<T>` (queryable) and the tagged
  form (`squeezy-parse/languages/ruby.rs`), and `include`/`prepend` added to the augment's
  `INHERITANCE_OPS` (`file_ops.rs`) so `include Sidekiq::Component` greps fire. Unit-tested.
- **GAP 4(a) (LANDED):** added a steer to `DEFAULT_INSTRUCTIONS` (`squeezy-core/src/lib.rs`):
  when a graph result gives a symbol_id, read its body with `read_slice(symbol_id, span_kind=body)`
  not whole-file `read_file`; for enumerate-then-inspect tasks, enumerate once via
  hierarchy/decl_search/reference_search then read only resolved spans. Targets the residual ts
  variance (a run reverting to the read-file storm) + java's read-storm. NOTE: global-prompt change
  → measure for regressions on already-winning langs.
- **GAP 4(b) (DEFERRED):** batch `read_slice(symbol_ids=[...])`. The agent confirmed the model
  already batches read_slice within ONE turn, so a batch tool saves only per-call args/reasoning
  (modest) for non-trivial risk in the security-sensitive read path. Designed; deferred unless
  measurement shows the steer (4a) is insufficient.
- **#6 (chain-trace depth cap):** subsumed — the graph BFS is already capped (MAX_GRAPH_MAX_DEPTH=8);
  the `deep_chain_expansion` finding is a heuristic for ≥4 read_slice/grep in a turn, addressed by
  the GAP 4(a) steer, not a new hard cap.

After A3 lands: re-measure FIX langs head-to-head; FIX-by-ratio + h2h-WIN = a kept graph-attributable
win. THEN iterate A2 tasks/repos for the rest (go/python/js likely need a larger same-lang repo).

### RESULT (round 2) — honest n=5 head-to-heads after GAP 1/2/3/4
- **ruby (mini): TIE → WIN.** GAP 3 flipped it: $0.061 vs codex $0.079 = **0.76, 100% recall** (n=5).
  Mechanism: the augment now fires on `include Sidekiq::Component` and `decl_search mixin:Component`
  resolves the mixers, so the model's grep-storm collapsed 48 → ~20 tool calls. Clean graph win.
- **ts (haiku): LOSS narrowed but NOT flipped.** At n=5: $0.209 vs CC $0.185 = **1.13** (was 1.26;
  100% recall). My earlier n=3 "WIN 0.62" was VARIANCE — corrected. GAP 1 is a real capability win
  (TS inheritance now graph-queryable, tighter reads on good runs) but ts is too high-variance to be
  a reliable win at n=5. Honest: GAP 1 helped, did not clinch ts.
- **java (mini): still a marginal LOSS, now on RECALL.** Cost IMPROVED (0.79× vs old 0.96×) but the
  aggressive GAP 4(a) steer ("read only resolved spans, don't whole-file read per item") dropped
  recall 100% → 94.4% (17/18) — the "cheaper because it did less" trap the plan forbids. => SOFTENED
  GAP 4(a) to just "graph id → prefer read_slice over read_file" (recall-neutral); re-validating.
- LESSON: a global-prompt steer that trades thoroughness for cost violates "cheaper AT EQUAL recall".
  Measure recall, not just cost, before shipping any steer.

### RESULT (round 1) — GAP 1 moved `ts` from a clear LOSS to a (variance-bound) WIN
- Pre-GAP1 (clean binary): ts h2h LOSS 1.26 (sqz $0.243 vs CC $0.193); with-graph did 25 whole-file
  `read_file`s + 0 inheritance queries because `decl_search base:` returned nothing for TS.
- Post-GAP1: with-graph dropped to ~$0.15–0.18 median (samples span $0.094–$0.237 — HIGH variance);
  the cheaper runs now use tight `read_slice` (graph symbol ids) over whole-file `read_file`
  (e.g. $0.094 = 19 read_slice + 2 read_file). One h2h n=3 read **WIN 0.62** (sqz $0.152 vs CC
  $0.244) but that CC sample was high ($0.244 vs the stable $0.193); against ~$0.21 CC, sqz ~$0.16
  is ~25% cheaper = a real but modest win. Graph-attribution no-graph-check (clean n=3):
  **ng/wg = 1.31** (wg $0.177 / ng $0.232), so the graph is provably doing the work, but the margin
  is modest and n=3 verdicts are unreliable — ESCALATE to n=5+ for the board.
- HONEST: GAP 1 is a correct, deterministically-tested CAPABILITY win (TS/JS inheritance is now
  graph-queryable); the benchmark flip is real but variance-bound, and the residual variance (a run
  still reverting to the 23-read_file storm) is GAP 4 (model must RELIABLY use the now-working graph).
- java/ruby unaffected by GAP 1 (they already had `base:` attributes; their loss is the read-storm
  = GAP 4, and Ruby `include` augment = GAP 3). C/Go have no class inheritance → GAP 1 N/A there.

### RESULT (round 3) — transitive decl_search works but the cost-win zone is NARROW
- transitive decl_search (committed): VALIDATED end-to-end. On a php Store-implementers task the
  model called `decl_search base:TaggableStore transitive=true` and hit 100% recall at $0.011 in
  5 tool calls — the capability is used and correct.
- BUT two redesigned tasks FAILED the no-graph gate by measurement (not assumption):
  - go/grpc-go (implicit interface): 0.524 — squeezy's graph doesn't resolve Go structural
    interfaces, so with-graph queries, gets nothing, greps anyway (overhead).
  - php Store (shallow transitive + co-located): 0.98 — ONE intermediate base (TaggableStore) that
    grep chases with one extra `extends TaggableStore` grep, and Cache/ is ~14 files (cheap to read).
    Both arms 100% recall at ~$0.015; graph vs grep is noise at that scale.
- PATTERN (go + php Store + A1): the graph beats grep on COST only in a NARROW zone — DEEP multi-level
  transitive (grep must chase many intermediates) AND members SCATTERED across many files AND a large
  repo. Shallow / co-located / small / implicit-interface tasks are grep-shaped. Grep is very efficient
  on well-organized code; the graph's composability (fewer tool calls) doesn't become a cost win unless
  grep is forced to be expensive.
- The one remaining cloned candidate in the decisive zone: dart RenderBox (RenderBox -> RenderProxyBox
  -> RenderProxyBoxWithHitTestBehavior -> ...; ~70-120 transitive scattered across rendering/widgets/
  material/cupertino; flutter is huge). Make-or-break deep test (big GT build + slow measurement).
- META: capability wins (JS/TS inheritance, Ruby mixins, transitive closure) are real PRODUCT wins;
  turning them into decisive BENCHMARK cost-wins on comprehension is hard. Consider reframing the demo
  to capability/composability + the few genuine cost-wins, or pivoting to the CODE-GEN track (Track B)
  where the graph's exhaustive-edit (recall-of-sites) value is more inherently decisive than cost.

### Harness note
Always rebuild release after a squeezy change: `cargo build --release -p squeezy-eval`. Drivers:
`.work/a1/a1_driver.py` (with/no-graph ratio), `.work/a1/confirm_h2h.py` (head-to-head verdict).
