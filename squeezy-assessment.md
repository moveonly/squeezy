# Squeezy: Strengths, Weaknesses & Gap‑Filling Ideas

> Assessment produced by a 34‑agent multi‑lens audit (15 subsystem audits, each adversarially
> verified against the code; 3 cross‑cutting lenses — competitive, thesis‑vs‑reality, code‑health;
> plus a completeness critic). ~3.2M tokens, ~26 min. Adversarial verification refuted only **1** of
> ~120 weakness claims and surfaced ~64 additional missed issues; corrected claims are reflected below.
>
> Repo: `squeezy-category-review-4` · ~511k LoC · 19 crates · Rust 1.93.1 · branch `fix/shell-diff-fold-by-default`.

**What it is:** a Rust coding‑agent TUI whose thesis is that *cost, speed, and code‑understanding* should be
enforced by the substrate — a persistent local tree‑sitter semantic graph, evidence‑packet tools, a cost
broker, and a tiered model router — rather than left to emerge from an unbounded loop.

**Bottom line:** The engineering is genuinely strong and the architecture is *correct for where the field
landed in 2026*. Maturity is high almost everywhere (one subsystem 5/5, most 4/5). But there is one
dominant, repeated meta‑flaw: **the marquee safeguards and the thesis‑defining wins are built, tested, and
then left unloaded — off by default, un‑instrumented, or un‑gated in CI.** The machine is impressive; the
ignition is disconnected.

## Maturity snapshot

| Subsystem | Score | Subsystem | Score |
|---|---|---|---|
| Agent loop / orchestration | 5 | MCP integration | 4 |
| Semantic graph & code understanding | 4 | Skills & hooks | 4 |
| Language coverage & parsing | 4 | Checkpoints / sessions / telemetry | 4 |
| Cost broker / budgets / receipts | 4 | Distribution / CI / release | 4 |
| Model routing / turn router | 4 | TUI architecture & rendering | 4 |
| LLM provider abstraction | 4 | TUI UX & feature surface | 4 |
| Tool surface & evidence packets | 4 | **Eval / benchmark / validation** | **3** |
| Shell sandbox / permissions / security | 4 | | |

---

## Strengths (verified)

**1. The semantic graph is a real, conservative, production‑grade differentiator.** The call/reference
resolver walks a long language‑aware ladder and, when no rule fires, returns `External`/`CandidateSet`
(capped at 8) rather than forging a wrong edge — with `// Bug #N` guards documenting deliberate refusals.
Reference binding is a ~20‑check precision funnel, not lexical grep. Persistence is engineered for warm
start (single‑fsync batched writes, fingerprint‑based parse‑skip, resolver‑cache rehydration). Accuracy is
gated in CI against *real compiler oracles* (rust‑analyzer, Roslyn, CPython ast, Prism, SemanticDB, Dart
analyzer). This is exactly the "hybrid lexical+structural, labeled‑confidence, never trust a stale index"
design that Sourcegraph/Amp arrived at the hard way.

**2. The cost machinery, when engaged, is real — not a metrics shim.** The broker's
`reserve_call`/`session_cap_reached`/`pressure_gate`/`round_input_gate_status` genuinely short‑circuit
dispatch. Context receipts are fully implemented and persisted, including a non‑obvious *negative‑receipt*
path (empty grep/glob collapses to a stub) and count‑from‑content reuse. Graph‑first navigation is
*enforced* — `read_file` is denied until graph evidence is seen. Prompt‑cache stability is sophisticated
(exploits Anthropic's 4th `cache_control` breakpoint as a stationary settled‑tail anchor; keeps the tools
index byte‑stable across rounds).

**3. Code health is unusually disciplined for an agent‑built codebase.** Across 321 prod files: only **13
`unwrap()` and 8 `panic!`** (survivors provably safe). The test suite is large *and behaviorally real* —
5,365 `#[test]` + 1,424 `#[tokio::test]`, ~21k asserts, and exactly **one** `assert!(true)` in the whole
workspace. CI enforces `clippy -D warnings`, cargo‑deny, a build‑script allowlist, and a committed 72%
coverage floor. `unsafe` is contained to platform FFI.

**4. Competitive positioning is a genuine, under‑served niche.** Nobody else makes a *deterministic cost
governor* the product thesis (competitors handle cost reactively via subsidized credit pools or
dashboards). Context receipts + failure memory are novel anti‑waste mechanisms. ~20 providers + true local
backends (ollama/llamacpp/lmstudio) match the OSS leaders. The local‑only, no‑server, BYO‑key posture is a
clean privacy/enterprise/air‑gap wedge. The agent loop itself scored 5/5.

---

## Weaknesses — organized by six cross‑cutting themes

### ① Inert‑by‑default safety — *the* headline finding
The same anti‑pattern recurs across **seven** subsystems: the safeguard is built and tested, then ships
disabled.

- **Cost caps are no‑ops out of the box:** defaults are 10,000 tool calls, **1 GB** bytes read, 1M files
  per turn ("sized so they never bind"), and `max_session_cost_usd_micros = None`. `enforces_result_budgets()`
  early‑returns when caps are at `u64::MAX`. Worse, `cost_warn_percent` is *fully inert without a cap* (it
  computes a % of a cap that doesn't exist), and the cap silently no‑ops on any unpriced/local model **even
  when explicitly set**.
- **Checkpoint rollback — the advertised undo — is OFF by default** (`checkpoints_enabled = false`), so a
  wrong edit has no Squeezy‑side undo unless the user opted in.
- **Subagent spend never advances the session cap** (verified, promoted from a missed issue): each subagent
  gets a *fresh* broker starting at $0, its loop calls zero cap checks, and `apply_subagent_dispatch` folds
  spend into a separate metrics bucket via a path that never touches `record_provider_cost`. A
  subagent‑heavy turn can run unbounded both mid‑run and post‑run. The subagent wall‑clock cap is also
  disabled when set to 0.
- **GPG release signing is always skipped** due to a step‑level `if:`/`env:` scoping bug — the `if` is
  evaluated before the step's `env` exists, so it always sees empty and skips, even when the secret is set.
  Releases ship unsigned while `install.sh` advertises signature verification (and no public key is ever
  provisioned anyway).
- **`squeezy-eval check` is wired into zero workflows**, and the input‑token regression gate has no
  committed baseline (short‑circuits to `Ok`).

### ② Measurement doesn't cover the thesis
The three thesis pillars are the *least* protected.

- **Cost has zero coverage in the only CI benchmark** — `tool_metrics_report` hardcodes
  `estimated_usd_micros: 0`. The headline "cheaper than Codex/Claude Code at recall parity" numbers live in
  `/tmp` graders against un‑vendored rival CLIs, are internally contradictory (CSV says 15/15 all‑WIN; the
  methodology doc beside it says 13/15 with C+Go losses; a re‑baseline says 9/11), and were last refreshed
  ~5 PRs before HEAD. Recall graders are loose (substring `\bname\b` anywhere in the answer), and the
  ground truth was itself recently buggy ("scored 0% for everyone").
- **Speed is not instrumented at all** — `THESIS.md` claims four tracked latency axes; there is **no TTFT
  and no per‑turn wall‑clock** anywhere in the running agent or telemetry. "Fast" is unfalsifiable in
  production. (Cold‑start and tool‑latency *are* architecturally addressed via lazy indexing + local graph.)
- The benchmark workflow only runs on a `benchmark` label, and `release.yml` runs **zero test/clippy/fmt
  gates** before publishing.

### ③ Built‑ahead‑of‑wired / the recommended API is the dead one
A systemic pattern where the most *documented* surface is unreachable:

- **`AgentHookBus`** — the crate doc's "contract new integrations should target" — has **zero production
  call sites**; the constructor never even installs it. The whole `HookResult.mutate` machinery (context
  injection, prompt rewrite) is live‑dispatched but **no production handler can populate it**.
- **The incremental‑refresh fix exists but is unwired:** every file edit triggers a full‑workspace
  cross‑symbol re‑resolution (`rebuild_semantic_edges` over *all* edges). The `compute_affected`
  (reverse‑import reachability) + Tarjan‑SCC scheduler that would scope this are fully built and tested but
  "pure data with no active consumer" — wired only into the `impact` query, not into refresh. This is the
  single biggest cost/speed gap in the steady‑state edit loop.
- **Worktree** primitive has zero callers but a doc promising a `/worktree enter|exit` command that doesn't
  exist. **Fork‑mode skills** and `SubagentKind::Skill` are parsed/rendered but never dispatched.

### ④ Stringly‑typed / LLM‑prose decisions at security & control boundaries
- The **auto‑approval reviewer** (the component sold as the safety gate) interpolates attacker‑controllable
  transcript content (prior tool outputs, file contents from a malicious repo) into its prompt with **no
  escaping/delineation**, then a cheap model decides approval — a direct prompt‑injection surface into the
  safety gate. It parses the model's free text by "first `{` to last `}`", and a deny is silently
  downgraded to a human prompt on a too‑generic substring (`"not auto-approved"`).
- **Structural hazards only escalate to Ask, never Deny.** `python -c "<arbitrary code>"` lands as
  `Shell/High`, within the auto‑allow ceiling — so the cheap reviewer **can auto‑approve arbitrary inline
  Python**, while `APPROVAL_POLICY.md` *falsely* tells the reviewer such commands are "denied before they
  reach you." (`sudo` is correctly blocked as Critical — that sub‑claim was refuted.)
- **Mid‑turn model escalation doesn't reset `previous_response_id`** at any of its **4** sites; under
  `store_responses=true` (OpenAI/Azure) the escalated request replays the cheap model's response id against
  a different model → can 400 / terminal‑fail. 19 other state‑change sites reset it correctly; this one was
  missed. No test covers routing × `store_responses`.
- Provider retry/error classification keys on substring matching; refusal‑based escalation is an
  **English‑only** 8‑phrase substring list (and the trust‑note prompt coaches the model to say "I'm not
  sure" — so escalation depends on the weak model obeying an English script).

### ⑤ TUI surface modality has no invariant
Independently surfaced by both TUI audits: there is no `ActiveOverlay` enum/stack. Modality is held
together by a 42‑arm key‑dispatch chain, a 26‑arm render chain, and an 8‑flag paste guard — **hand‑synced
by review**. Toggles set only their own flag (no `close_other_overlays()`), so two overlays can be "open"
at once. Concrete latent bug: the two paste paths gate *different* overlay sets, so an image paste under
any of 7 overlays inserts a ghost token into the composer beneath the overlay. (The earlier "god‑object
with 1319 methods" framing was corrected by verification — it's a free‑function architecture over a shared
282‑field struct, not one giant impl. The maintainability concern stands; the characterization was off.)

### ⑥ Monolith + bus‑factor of one
`squeezy-tui/src/lib.rs` is **49,963 lines** (and *grew 2.6×* after a decomposition plan was marked
"Executed"); agent `lib.rs` is 18,607. There's no `[workspace.lints]` table and no file/module‑size budget,
so local `cargo clippy` doesn't match CI and nothing prevents the next 50k‑line file. Git authorship is
effectively **one person** (two identities sharing one email). The recurring "resolve 37/27/21/195 bugs
from multi‑agent audit" commits signal a ship‑first‑fix‑in‑bulk cadence rather than correct‑on‑first‑write.

### Honesty note — claims corrected via adversarial verification
So the rest can be trusted: several first‑pass findings were refuted and dropped/rewritten.

- **Document attachments are NOT silently dropped** — every native provider calls
  `reject_unsupported_documents` and fails loudly; the "skipping" branches are dead happy‑path code.
- **The `unified_diff` path‑escape does NOT reach `.git/` or `../`** (git apply blocks those). The real,
  narrower escape is in‑repo non‑`.git` paths including secrets and `.squeezy`/`.agents`.
- **Routing DOES have a dollars‑saved measurement** (`routing_estimated_savings_usd_micros`); the "no
  closed‑loop measurement" gap holds only for the dedup/packing wins.
- **The language‑accuracy gate risk is INVERTED:** the *flagship* langs
  (rust/python/java/c#/js‑ts/c‑family) have **no** accuracy gate, while go/kotlin/php/ruby/scala/swift/dart
  do (Go is the most strictly gated — zero fp/fn).
- **Headless mode already exists** (`--prompt @file/stdin`, `--format json` with a final `Completed{cost}`
  record) — it's just unmarketed and the schema is labeled "experimental."
- **"Lazy hydration"** is eager full‑graph hydration in RAM (real memory‑scaling ceiling), but it's not a
  doc contradiction; and only `Confidence::Stale` is a dead producer variant (with a phantom tool‑boundary
  filter that can never match) — `Confidence::Partial` is heavily produced.

---

## Ideas to fill the gaps (prioritized)

### A. Make the thesis *true by default* and *provable* (highest leverage)
1. **Ship a binding "guardrails" cost preset — at minimum for non‑TTY/`--prompt`/headless runs** (S, high).
   Lower the inert 10k/1GB/1M defaults to realistic values and/or set a session cap when stdin isn't a TTY,
   where a runaway is most dangerous and least observed. Add a `squeezy doctor` warning when the cap is
   unset. Threads the "inert by default" problem without changing interactive UX.
2. **Make subagent spend advance the parent session‑cap basis** (S, high). Have `apply_subagent_dispatch`
   call `record_out_of_band_session_cost`, and add a periodic cap check inside the subagent round loop.
   Closes the unbounded‑spend hole.
3. **Publish a reproducible cost‑win benchmark and gate it in CI** (L, high). Run the existing paired
   graph‑vs‑nograph scenarios (a deterministic mock‑provider variant works for CI) on the *same* model,
   assert `with-graph tool_calls/input_tokens ≤ no-graph`, and persist a baseline. Wire `squeezy-eval
   check` into a workflow. Converts the entire pitch from architecture‑marketing to evidence — *the single
   most important adoption move* for a cost‑first tool.
4. **Instrument TTFT + per‑turn wall‑clock** (M, high). Stamp first‑token and turn‑end at the stream
   boundary; emit to telemetry and the status line. Makes 2 of the 3 thesis pillars observable instead of
   unfalsifiable.
5. **Add a live per‑session cost receipt / savings ledger to the TUI** (M, high). The data already exists
   (`CostSnapshot`, receipt‑stub metadata, `TurnMetrics`): "this session — 41k tokens, $0.12, 3 oversized
   rounds prevented, 7 re‑reads served as stubs." Merchandises the invisible differentiator;
   screenshot‑worthy.

### B. High‑severity correctness & security
6. **Reset `previous_response_id` at all 4 escalation sites** and extract the copy‑pasted ladder into one
   `Agent` method; add a routing×`store_responses` test (S→M, high).
7. **Harden the auto‑approval reviewer** (M, high): delineate/escape the attacker‑controllable transcript
   in the prompt; replace the first‑brace/last‑brace parse and the substring deny‑downgrade with structured
   output; make `python -c` (and other opaque inline‑code wrappers) either Deny or always‑Ask, and
   reconcile `APPROVAL_POLICY.md` with what the code actually enforces (add a doc‑vs‑code contract test).
8. **Validate `unified_diff` fallback bodies against the safety floor** (M, high): parse the `+++` targets
   and intersect them with the declared path, secret‑path, and protected‑metadata checks before invoking
   `git apply`.
9. **Fix or remove the dead GPG signing path, and make `release.yml` depend on CI status** (S, high): no
   commit with failing tests/clippy should be publishable. Add SBOM/build‑provenance attestation while
   you're there.

### C. Structural debt (compounding payoff)
10. **Introduce an `ActiveOverlay` enum / overlay‑stack** with enforced mutual exclusion and a single
    source of truth for active‑surface + paste suppression (M, high). Dissolves an entire latent‑bug class.
11. **Split the 50k‑line `lib.rs` monoliths** into submodules (the TUI already has 117 `mod` decls, so most
    logic moves out of the root cheaply), and add a **`[workspace.lints]` table + a file/fn‑size budget**
    so local clippy matches CI and 50k‑line files can't recur (L + S, high).
12. **Wire `cargo-mutants` nightly on the core crates** (M, high) to convert 72% line coverage into a
    verified assertion‑strength signal — important given ~200k LoC of agent‑written tests.
13. **Wire `compute_affected` + the SCC scheduler into `refresh`** (L, high) for incremental cross‑symbol
    re‑resolution instead of a full‑graph rebuild on every edit. The hard algorithm is already built and
    tested.

### D. Parity, recall, and ecosystem
14. **Implement Gemini explicit context caching** (M, high) — the registry already advertises
    `prompt_caching: true` for all Gemini models but the request path emits no cache directive; it's the
    largest unclaimed cost lever on a major provider. Also wire `ContextOverflow` recovery on
    Google/Bedrock/Ollama/compatible, and honor `content_parts` images on Anthropic+Bedrock (currently
    reverts tool‑image results to base64‑in‑string bloat).
15. **Add a recall‑regression gate over `squeezy-rank`** and **add accuracy gates to the flagship
    languages** (M, high). The BM25/fuzzy rank layer shapes recall — the headline metric — yet has zero
    oracle/gate; and the six most‑used languages have no accuracy gate at all.
16. **Finish user‑definable subagents** (dispatch the `.squeezy/agents/*.md` catalog with per‑subagent
    model selection — naturally pairs with the router: cheap model for the explorer subagent), and
    **resolve the dead APIs**: wire or demote `AgentHookBus`; wire or delete the worktree module (L, high).
    Closes the most visible extensibility gap vs Claude Code without adopting a marketplace.
17. **Stabilize and *market* the existing headless mode** (S→M, high): version the `--format json` line
    schema, add a golden conformance test, and make "`squeezy --prompt --format json` with a hard cost cap"
    the agents‑in‑CI story. Re‑uses what's already built.
18. **Reconcile telemetry's opt‑out default with the privacy wedge** (S, medium): a TUI first‑run consent
    surface (or opt‑in default) — an enterprise evaluating the "local‑only, no data leaves" positioning
    will find data leaves by default.

---

## If I had to pick one thing
The gap between Squeezy's quality and its perceived value is almost entirely items **A1–A5**: turn the
broker on by default, count subagent spend, and *prove the cost/speed wins with a checked‑in, CI‑gated
benchmark and a visible per‑session ledger*. The hard engineering is already done and verified; what's
missing is connecting it to the default experience and to a number a buyer can see.
