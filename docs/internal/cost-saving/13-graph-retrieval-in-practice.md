# 13 — Graph retrieval in practice: build cost, streaming robustness, read routing

Chapter 05 describes *why* the semantic graph is cheap in principle — signature-only slices, BM25-ranked retrieval, incremental re-parse — instead of dumping whole files. This chapter documents the work that made that principle hold up on real repositories under a cheap model, where it had quietly stopped paying off. Three things were eating the saving: the graph was too slow to build (so the agent fell back to grep), a streaming-reconnect bug was burning whole turns, and on single-file tasks the agent over-used the graph and paid a navigation tax with no benefit. Each is a cost mechanism in its own right, and each is auditable from `file:line`.

The benchmark snapshot captured in `docs/internal/eval-findings/graph-cost-wins-report.md` reported that, at the time it was run, squeezy beat fresh Codex (gpt-5-mini tier) on **9 of 12 measured languages (1 tie, 0 losses)** and fresh Claude Code (Haiku tier) on **7 of 9 (0 ties, 2 cost-losses under investigation)**. Treat those numbers as historical eval evidence, not a current provider-price claim; rerun the report before citing live competitiveness.

## Mechanism

### 13.1 Build cost: the graph has to be cheaper to build than the grep it replaces

The graph only saves money if it is *available* by the time the model needs it. If the build is slow, the agent waits — or worse, times out the wait and falls back to `grep`/`read_file` scans, paying the full byte cost the graph was supposed to avoid. On several languages the build had degraded to tens or hundreds of seconds because of accidental re-traversal.

Two classes of fix:

- **Visited-set discipline in cross-reference resolution.** Several language resolvers walked the container/ancestor hierarchy without a visited set, so a diamond in the type/method graph was re-expanded exponentially. The fixes add a per-walk visited set in the affected language modules — see `crates/squeezy-graph/src/languages/dart.rs`, `python.rs`, and `ruby.rs` (the `visited` guards around ancestor/method resolution). PHP was fixed in the shared resolver path rather than its language module.
- **A language-partitioned symbol index.** `symbols_by_language_identity` in `crates/squeezy-graph/src/resolution.rs` (consumed from `crates/squeezy-graph/src/lib.rs`) buckets symbols so identity resolution scans only same-language candidates instead of the whole symbol table.

The combined effect is large: representative build times dropped from **117s → 4s** (a Laravel-scale PHP repo) and **332s → 29s** (a Flutter-scale Dart repo), and the resulting graph is byte-identical to the slow path — verified with the offline harness in `crates/squeezy-graph/examples/graph_build_timing.rs`. Below the wait ceiling, the agent now actually gets the graph instead of falling back.

That wait ceiling is itself a knob. `graph_ready_wait()` in `crates/squeezy-tools/src/lib.rs:174` defaults to 30s (overridable via `SQUEEZY_GRAPH_READY_WAIT_MS`) — long enough that a freshly-opened large repo finishes building before the first retrieval, short enough that a pathological build doesn't strand the turn.

### 13.2 Streaming robustness: a dropped reconnect must not re-bill the turn

A provider stream can drop mid-response; `with_stream_retry` in `crates/squeezy-llm/src/retry.rs` reconnects and replays, skipping the prefix it already validated so the user sees one continuous answer. The skip is done by `skip_validated_prefix` (`retry.rs:711`), which advances a `seen` cursor over the bytes already forwarded.

The cursor advance was wrong: it counted only the consumed prefix and dropped the forwarded-suffix length, so after a reconnect the cursor under-counted and the validator saw a *divergence* where there was none. Under a cheap, chatty model that reconnects often, this surfaced as "stream reconnect diverged" failures and, in the worst case, turns that produced no billable output at all — pure waste, paid for and thrown away. The one-line fix at `retry.rs:736`:

```rust
*seen += consumed + forwarded.map_or(0, |suffix| suffix.chars().count());
```

now advances the cursor by *both* the consumed prefix and the forwarded suffix, so the replay lines up and the turn completes once. This is a reliability fix, but it is squarely a cost fix: a turn that has to be retried from scratch is a turn billed twice. (It was also a hard regression — the bug had silently disabled the entire Haiku tier on this benchmark until it was traced and fixed.)

### 13.3 Slice padding and graph-packet compaction

Two smaller shapers keep graph retrieval from leaking bytes:

- **Auto-widened slices.** A `read_slice` that asks for fewer than `READ_SLICE_AUTO_WIDEN_THRESHOLD_LINES` (40) lines is padded up to `READ_SLICE_AUTO_WIDEN_TARGET_LINES` (48) — `crates/squeezy-tools/src/graph_tools.rs:2117`. A too-tight slice that clips the function the model wanted forces an immediate second `read_slice` for the surrounding lines; each follow-up re-bills the whole growing prompt. Padding to a sane window trades a few cheap bytes now for a saved round-trip.
- **Compactable graph packets.** `definition_search`/`symbol_context`/`hierarchy`/`read_slice` results are listed in `COMPACTABLE_TOOL_NAMES` (`crates/squeezy-agent/src/micro_compaction.rs:40`) so a navigation packet the model has already consumed is eligible for the same receipt-stub treatment as a file read (Chapter 03), instead of riding in the prompt for the rest of the session.

### 13.4 Read routing: don't pay the graph tax when one file holds the answer

The graph wins when the answer is *spread across files* — call sites in one module, the definition in another, the type three files away. It loses when the answer is concentrated in a single small file: navigating symbol-by-symbol with `read_slice` then costs more than reading the file once, because each slice is a separate turn and every turn re-bills the conversation so far.

This was measured, not assumed. On the Python realworld scenario (whose answer lives entirely in one `sessions.py`), squeezy issued **20 `read_slice` calls across 12 turns** where Claude Code answered in **one `Read` plus one `Bash`, 3 turns** — and paid ~1.8× for it. The cost is the serial-turn re-billing, not the bytes.

The routing heuristic (`crates/squeezy-tools/src/graph_tools.rs`, with the prompt-side nudge in `crates/squeezy-agent/src/lib.rs`) steers the agent to read a whole small file once when it would otherwise slice many symbols out of the same file, while leaving genuine cross-file navigation — the case the graph exists for — untouched. The guard is "same file, small file, several slices"; cross-file retrieval still goes through the index. See Chapter 05 for the retrieval surface this sits on top of.

## Cost intuition

The four mechanisms target different lines of the bill from the taxonomy in the README:

| Mechanism | Bill line it cuts | Rough effect |
|---|---|---|
| 13.1 Build cost | Recomputation / grep fallback | Graph available instead of full-file scans; build 28–38× faster on large repos |
| 13.2 Stream cursor fix | Recomputation across turns | Eliminates double-billed retried turns; unblocked an entire model tier |
| 13.3 Slice padding | Tool-output bytes + extra turns | Saves the follow-up `read_slice` round-trip on clipped windows |
| 13.4 Read routing | Recomputation across turns | Avoids N serial slice-turns when 1 file read suffices |

These are validated end-to-end rather than per-mechanism: the realworld A/B (`with-graph` vs `no-graph`, fresh same-day baselines for both Codex and Claude Code, same provider rates on both sides) is the integration test. See `docs/internal/eval-findings/graph-cost-wins-report.md` for the per-language table and the consistency methodology (n≥8 where a result was close, since n=3 proved too noisy to rank a near-tie).

## Edge cases & limits

- **Build speedup is a correctness-preserving optimization, not a heuristic.** The visited-set and index changes produce a byte-identical graph (asserted in the timing harness); they remove redundant work, they don't approximate. A regression here shows up as a wrong graph, not just a slow one, so the equality assertion is the guardrail.
- **The read-routing heuristic must not discourage cross-file graph use.** Its trigger is deliberately narrow (same small file, repeated slices). Widening it risks pushing the agent back to whole-file reads on genuinely cross-file tasks — exactly the regression the graph was built to prevent. It is reversible via config if behaviour-risky.
- **Single-file vs cross-file is task-shaped, not language-shaped.** Whether the graph pays off depends on how spread out the answer is, not on which language it's in. A 2-grep task does not justify the graph's build-and-navigate tax; a 50-grep cross-module task does. The routing heuristic encodes that, but the benchmark scenarios still have to be chosen so the task is big enough for the mechanism under test to matter.
- **Two Haiku cost-losses were open in the report snapshot.** Python (post-routing-fix) and a borderline Ruby result were the languages where squeezy was not cheaper than Claude Code at the Haiku tier in that run; both are tracked in the report rather than papered over. Honest accounting first: a loss in the table is a loss.
