# Graph cost-win investigation — findings, fixes, and validation status

Goal: make squeezy **consistently cheaper than CC-Haiku and Codex-Mini while
keeping recall ≥ 95%** on the 15-language realworld graph-vs-nograph eval,
prioritising (1) graph-code bugs/heuristics, (2) task/scenario design, (3)
generic cost optimisations — and investigating, not assuming, every step.

Branch: `perf/graph-cost-wins` (stacked commits, pushed). This report records
what was measured, what was changed, what is validated, and — importantly —
the environmental blocker that prevented live cost re-measurement in this
session, with a runbook to finish it.

---

## 0. TL;DR

- **Consistency analysis (done):** decomposed every WIN/LOSS in both CSVs into
  recall-vs-cost causes, computed the graph "tax" (with-graph vs no-graph cost),
  and quantified per-rep variance. Two languages lose on **both** tiers (`cpp`,
  `python`); five **flip** by tier; the verdict itself is noise-dominated.
- **Root causes found and confirmed empirically** — and they differ from the
  earlier one-line guesses:
  - The graph **tax is context accumulation**, not just packet size: with-graph
    makes many small graph calls whose packets persist in context and are
    re-billed every turn.
  - `php`/`dart`/`scala` have `graph_available=false` on **100%** of calls — but
    for **three different reasons**: `php` = the 5 s readiness wait races a cold
    index build; `dart` = a genuine **exponential** ancestor-walk that never
    finishes; `scala` = indexing simply slower than 5 s.
  - The `$0`-failure/variance that makes verdicts unreproducible is largely a
    **brittle stream-reconnect** that fails the whole turn on an early divergence.
- **Five fixes implemented, committed, and validated offline / by unit tests**
  (Waves 0–5a below). The dart fix is offline-validated on the real Flutter repo.
- **Live cost re-measurement is blocked** by an environmental issue: reqwest's
  HTTPS streams to Anthropic reset ~1 token in (≈100% right now), while raw
  `curl` with the identical request shape streams cleanly. This is **not the
  product code** (the unmodified `main` binary fails identically) and not
  account throttling (`curl` works; 4 h of rest did not clear it). See §4.

---

## 1. Consistency analysis (deliverable #1)

Verdict rule (from `README.md`): **WIN** iff `wg_recall ≥ 95%` **and**
`wg_cost ≤ 0.95 × baseline`; **TIE** if recall passes and cost within ±5%;
**LOSS** otherwise. `graph_tax% = (wg_cost − ng_cost) / ng_cost × 100`
(positive = the graph is *more* expensive than squeezy's own no-graph arm).

| lang | Haiku vs CC | loss cause | tax% | Mini vs Codex | loss cause | tax% | cross-tier |
|---|---|---|---|---|---|---|---|
| c | WIN | — | −76 | N/A | — | −8 | mixed (no codex base) |
| **cpp** | **LOSS** | cost | +33 | **LOSS** | cost | +31 | **consistent loss** |
| csharp | TIE | — | +27 | WIN | — | +17 | mixed |
| dart | LOSS | **recall 66.7** | −40 | WIN | — | −18 | flip |
| go | WIN | — | +239 | TIE | — | +5 | mixed |
| java | WIN | — | +30 | LOSS | cost | +27 | flip |
| js | WIN | — | +33 | WIN | — | −10 | **consistent win** |
| kotlin | LOSS | cost | +6 | WIN | — | −1 | flip |
| php | LOSS | cost | +58 | TIE | — | +13 | mixed |
| **python** | **LOSS** | cost | +94 | **LOSS** | **recall 67.5** | +21 | **consistent loss** |
| ruby | TIE | — | +99 | WIN | — | +1 | mixed |
| rust | WIN | — | −6 | LOSS | recall 93.8 | +27 | flip |
| scala | WIN | — | +33 | LOSS | cost | −23 | flip |
| swift | WIN | — | +7 | WIN | — | +7 | **consistent win** |
| ts | LOSS | cost | −3 | N/A | — | +51 | mixed |

**Three structural findings:**

1. **The graph is pure overhead on "easy" tasks.** Where no-graph already hits
   100% recall, with-graph adds **+25% to +240%** cost for *zero* recall gain
   (python-haiku +94, go +239, ruby +99, php +58, cpp +31–33). The graph earns
   its keep only where no-graph recall *collapses* (rust-haiku ng→68.8, js-haiku
   ng→70.5, c, dart-mini): there it is cost-negative and load-bearing.

2. **The mechanism is context accumulation, not raw packet size.** Measured
   directly: python-haiku with-graph makes **25 tool calls** vs no-graph's
   **3**, yet costs *more* ($0.087 vs $0.075). Each graph packet (verbose nested
   spans, fully-qualified `id`s like `src/requests/sessions.py::class:Session@14813`,
   signatures) persists in context and is re-billed on every later prefill, so
   cost scales with packets × turns.

3. **The verdict is noise-dominated.** Per-cell cost spreads reach **40×**
   (go-haiku with-graph reps span $0.008–$0.32), `$0`-failure rates hit 50%+ on
   some cells (java-mini ng: 3 zeros; the median silently drops them), and a
   3-rep resample flips ~14 of 26 cells. `ts`-haiku (tax −3, still loses) and
   `scala`-mini (graph *cheaper* than ng, still loses) reveal a residual
   generic-cost gap vs the baselines that no graph change can close — those are
   bucket-2 (scenario) problems.

**Targets, in priority order:** (1) `cpp`+`python` (consistent losses; one fix
clears two cells each), (2) `php`/`dart`/`scala` (graph never available — see
§3.2), (3) the high-tax bypass cluster (`kotlin`, `java-mini`, `ruby`), (4)
reduce variance so wins are reproducible (the stream-reconnect fix, §3.5).

---

## 2. How the cost is actually spent (transcript evidence)

From the raw `target/eval/.../trace.jsonl` of successful runs:

- **Bypass + batch-read (cpp/php/java/kotlin):** the model is *given* the 12
  graph tools (paying their spec weight every turn) but makes only ~4–5 graph
  calls, then `read_file`/`read_slice`/`grep`-storms (cpp: 4 graph calls vs 126
  read_file + 31 read_slice + 16 grep). It pays the tax without the benefit; the
  no-graph arm would win.
- **read_slice explosion (python):** uses the graph (12 calls) then issues **63**
  `read_slice` calls, each auto-widened far past what was asked.
- **graph never available (php/dart/scala):** every graph call returns a
  fallback stub — the "with-graph" arm is silently "no-graph + a per-turn graph
  spec tax + wasted stub calls".

---

## 3. Fixes implemented (branch `perf/graph-cost-wins`)

Each is a separate commit so the PR stacks cleanly. **Validation status is
stated honestly** — most cost effects are evidence-backed but **not yet
live-measured** because of the §4 blocker.

### Wave 0 — `disable_prompt_cache` flag · `ee681900` · ✅ unit-tested
Env `SQUEEZY_DISABLE_PROMPT_CACHE` threads a hard cache off-switch into every
`LlmRequest`: `effective_cache_retention()` short-circuits to `None` (no
`cache_control`/`cachePoint`, no `prompt_cache_retention` on any provider), and
the OpenAI path injects a per-request nonce into the `instructions` prefix +
a unique `prompt_cache_key` to bust the un-disable-able automatic prefix cache.
**Why:** caching masks the true graph tax and is a prime suspect for the 40×
rep variance (warm vs cold cache); uncached A/Bs are deterministic and honest.
Unit test asserts no `cache_control` survives the flag (and the positive path
is unchanged).

### Wave 1a — recall-neutral cost cuts · `c5691922` · ✅ compiles, logic-safe
- **read_slice auto-widen 80/60 → 48/40 lines** (`graph_tools.rs`). read_slice
  is the highest-volume tool; Haiku's median request is ~20 lines but was padded
  toward 80 (a 2–4× over-read). The caller's requested range is **always fully
  included**, so recall cannot regress; this only trims context.
- **Graph tools added to `COMPACTABLE_TOOL_NAMES`** (`micro_compaction.rs`). A
  single `definition_search`/`symbol_context` packet runs tens of KB and pins
  into context, re-billed every later prefill. They were excluded under a
  (verified-false) comment that graph tools "return small payloads".
  Micro-compaction keeps the newest result verbatim and leaves a re-issuable
  receipt, so this is the same risk profile already accepted for `read_file`.

### Wave 2 — graph-ready wait 5 s → 30 s, env-overridable · `938086ec` · ✅ compiles
`GRAPH_READY_WAIT` → `graph_ready_wait()` (default 30 s, override
`SQUEEZY_GRAPH_READY_WAIT_MS`). The condvar fires the instant the background
index build completes, so fast repos pay ~nothing; the bound only bites on a
large *cold* repo. The old 5 s left php (index builds in ~5 s) and scala
stranded on `graph_unavailable`/`graph_indexing` for the whole session,
silently degrading the with-graph arm to grep. **Confirmed:** php/dart/scala
have `graph_available=false` on 100% of recorded calls.

### Wave 3 — Dart exponential ancestor-walk fix · `a3c21ffd` · ✅ unit-tested + offline-validated
`dart_method_in_ancestors` recursed through every same-named ancestor candidate
with **no visited-set**, re-expanding shared ancestors on every path. On
Flutter's diamond `State`/`Widget`/`Element` hierarchies this is combinatorial:
`GraphManager::open` ran **5+ minutes at ~10 GB and never returned**, so the
whole dart with-graph arm fell back to `graph_unavailable`. Fix threads a
`visited: HashSet<SymbolId>` through the recursion; an ancestor's subtree is
identical regardless of the path that reaches it, so the first match in Dart's
mixin→extends→implements→on order is unchanged. **All 10 dart graph unit tests
pass**; offline `GraphManager::open` on the real flutter/flutter repo now
completes in bounded time (see §5).

### Wave 5a — stream-reconnect robustness · `7d9113fc` · ✅ unit-tested
When a provider stream drops **before any visible output is committed** (only
`Started`/reasoning emitted) and the reconnect samples divergent text,
`with_stream_retry` surfaced `"stream reconnect diverged"` and failed the whole
turn to `$0`. Generation isn't pinned, so an early drop diverges almost every
time — this is the **dominant `$0`/variance source under load** (the original
parallel 15-lang sweep). Now an early divergence discards the partial and
**restarts the stream from scratch** within the existing retry budget; only a
*late* divergence (after committed text/tool-call) stays fatal. New unit test
covers it; existing late-divergence/replay tests still pass. Also adds
`SQUEEZY_FORCE_HTTP1` as an escape hatch (see §4).

---

## 4. The measurement blocker (fully diagnosed, not the product code)

Every attempt to live-measure failed with
`provider stream failed: stream reconnect diverged`. The investigation:

| test | result |
|---|---|
| squeezy `main` binary (no changes) | **fails** identically |
| squeezy branch binary (debug & release) | fails |
| raw `curl` simple stream | **works** |
| raw `curl` + thinking + 3000 tokens | works |
| raw `curl` + 15 tools + interleaved-thinking beta (squeezy shape) | works |
| 4 hours of API rest, then retry | still fails |
| force HTTP/1.1 (`SQUEEZY_FORCE_HTTP1=1`) | still fails |
| 8 rapid runs | **0/8** succeed |

**Conclusion:** the streams are reset ~1 token in by something between reqwest
and the Anthropic edge (TLS stack / socket behaviour — *not* HTTP version, *not*
request content, *not* the account, *not* squeezy's code: the unmodified `main`
binary fails the same way and `curl` with the identical request shape works).
It began ~05:55 local and persisted >4.5 h. The Wave 5a fix correctly restarts
on each early divergence (observable as more reasoning deltas per run) but
cannot beat a ~100% reset rate.

A background probe (`/tmp/graph-cost-wins/recover_probe.sh`) retries every 4 min
and, the instant a run bills >$0, captures the python/cpp/php nocache A/B to
`/tmp/graph-cost-wins/recover_probe.log` — so data lands automatically if the
environment heals.

---

## 5. Validation runbook (to complete when the transport recovers)

Measurement harness: `MAXW=1 python3 /tmp/graph-cost-wins/ab.py <lang> <haiku|mini> <n> [nocache]`
— runs N reps of each variant, grades recall via `/tmp/codex-runs/realworld`
graders, prints median cost + recall + graph_tax. Always pace (`MAXW=1`).

Validate in this order (iterate-then-scale):
1. **Wave 0 sanity:** python no-graph cached vs `nocache` — confirm uncached
   costs more (flag works end-to-end), then use `nocache` for all A/Bs.
2. **Waves 1a + 5a on the graph-available losers:** `python`, `cpp` (haiku).
   Expect the tax to drop (leaner read_slice + compacted packets) and recall to
   hold at 100. Target: LOSS→WIN.
3. **Wave 2 + 3 on the graph-broken set:** `php`, `scala`, `dart` (haiku).
   Re-confirm `graph_available=true` in the new traces, then recall/cost.
   dart-haiku recall should climb off 66.7.
4. Re-run the full sweep with **≥10 non-$0 reps/cell** and report a **flip
   probability**, not a point verdict (§1 finding 3).

Offline (no API) validation already runnable:
`cargo run -p squeezy-graph --example graph_build_timing -- /tmp/graph-repro/flutter`
(and `…/laravel`) — proves the dart build completes in bounded time and php
builds fast (motivating Wave 2).

---

## 6. Remaining work (deferred — needs live measurement to tune safely)

- **B1.3 EnumerateDeclarations planner intent**: route "every/all X under
  `<dir>`" prompts to `decl_search(path,kind)` instead of junk-anchor
  `definition_search`/`hierarchy` (the bypass cluster: cpp/kotlin/php/java).
  Strongly transcript-backed; medium risk → validate on cpp+kotlin first.
- **B1.4 adaptive raw-read guard**: keep the read guard active when the planner
  produced *real* graph evidence (steer to `read_slice`-by-symbol before
  whole-file reads). Medium recall risk.
- **B1.5 packet slim**: drop redundant top-level `spans`/dup `confidence`/`tool`
  and omit empty `symbol_context` arrays (~23% off each symbol packet). Low risk
  but needs a serialization-snapshot + smoke check.
- **B3.1 eager read-snapshot dedup**: persist `(path,start,end,sha)` after each
  read (not only at compaction) so the existing F03 dedup actually fires
  in-run (php had 95 repeat reads, 4 stubs).
- **Do NOT** lower `DEFAULT_GRAPH_MAX_RESULTS` (model overrides it >90% of calls)
  or rework prompt caching (already correct) — both refuted by evidence.

## 7. Failed/abandoned experiments

- **`SQUEEZY_FORCE_HTTP1`**: added to test whether reqwest's HTTP/2 was the
  reset cause. It was **not** (HTTP/1.1 also resets). The knob is kept as a
  harmless, genuinely-useful escape hatch for HTTP/2-hostile networks, but it
  did **not** unblock measurement here. Documented so it isn't mistaken for a
  fix to the §4 blocker.
