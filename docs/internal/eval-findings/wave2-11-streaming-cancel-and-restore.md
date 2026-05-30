# wave2-11 streaming-cancel-and-restore

Domain agent for wave-2 / 11. The probe shape is identical across the
three providers: emit a long-stream list prompt, cancel mid-stream the
instant the assistant text crosses `5.`, then send a follow-up that asks
the model to confirm restoration. The findings below are framed against
the wave-2 rubric in `docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`.

## Runs

| Provider | Scenario | Run dir | Result |
|---|---|---|---|
| openai | `wave2-11-streaming-cancel-openai.toml` | `target/eval/wave2-11-streaming-cancel-openai-1780145063183/` | completed: turn 1 cancelled cleanly, turn 2 emitted `restored: ready for next prompt` |
| anthropic | `wave2-11-streaming-cancel-anthropic.toml` | `target/eval/wave2-11-streaming-cancel-anthropic-1780145100441/` | completed: turn 1 cancelled cleanly, turn 2 emitted `restored: ready for next prompt` |
| portkey | `wave2-11-streaming-cancel-portkey.toml` | (no run — provider config error) | aborted before producing a run dir; see Finding 6 |

Both successful runs produce zero auto-findings in `findings.jsonl`. The
findings below are authored by hand from `trace.jsonl` and
`frames.jsonl` against the rubric.

A preliminary issue blocked every run: all three scenario files on main
shipped with a `wait_for` syntax that does not parse. That fix is
captured as Finding 3 — the runs above were taken **after** patching the
three scenarios to the externally-tagged form.

## Finding 1 — cancelled-turn cost reports `0/0/0` despite real provider work

- Severity: **major (P1)**.
- Rubric dimension: 2 (functionality) and 6 (cross-provider consistency).
- bd id: `squeezy-llaj`.
- File: `crates/squeezy-agent/src/lib.rs:5176-5184` (also synthetic Cancelled emit at `crates/squeezy-agent/src/lib.rs:166`).

`next_llm_stream_event` synthesizes an `LlmEvent::Cancelled` with no
usage payload on cancellation. The response-round loop only merges
`completed_cost` after a successful round (`lib.rs:5168-5174`); the
cancel branch calls `finish_cancelled_turn(&total_cost, ...)` with the
existing total, which is zero when the cancel arrives during the first
round.

Evidence on both providers:

- `wave2-11-streaming-cancel-openai-1780145063183/frames.jsonl` line 1:
  ```json
  "input_tokens":0,"output_tokens":0,"cost_micro_usd":0,"cost_display":""
  ```
  yet `trace.jsonl` contains 84 `reasoning_delta` and 45
  `assistant_delta` events for the cancelled turn (seq 6 → seq 136).
- `wave2-11-streaming-cancel-anthropic-1780145100441/frames.jsonl` line
  1: same 0/0/0 even though Haiku streamed five list lines plus a
  thinking trace.

Every cancelled turn is free in `run.json:totals`. For cost-cap
warnings and cost-warning bd tickets that key off the running total,
this is a silent under-count.

## Finding 2 — partial assistant text is dropped from conversation_state on cancel

- Severity: **major (P1)**.
- Rubric dimension: 2 (functionality).
- bd id: `squeezy-3hr4`.
- File: `crates/squeezy-agent/src/lib.rs:5176-5184` vs.
  `crates/squeezy-agent/src/lib.rs:5190-5197`.

The non-cancel fallthrough at `lib.rs:5190` calls
`flush_assistant_stream` and pushes the assistant message into
`conversation_state.conversation`. The cancel branch returns early
without that flush — the partial assistant text just goes out of scope.

Evidence:

- `wave2-11-streaming-cancel-openai-1780145063183/trace.jsonl`, the
  `turn_completed` envelope on the recovery turn (seq 145) carries
  `"context_estimate":{"items":2}`. Exactly two items: user-1 + user-2.
  The partial assistant output ("1. Add … 5.") is gone.
- `wave2-11-streaming-cancel-anthropic-1780145100441/trace.jsonl`
  seq 32: `"context_estimate":{"items":3}`. Haiku injected a
  cache-breakpoint placeholder so the count is 3 instead of 2, but the
  partial assistant text is still missing.

User impact: the `Acknowledge the cancel` follow-up lands on a model
with no in-conversation evidence that anything was cancelled. The model
"acknowledges" because we said so in the prompt, not because it
remembers the cut-off. In a real TUI session, Ctrl+R restores the
user's typed text (`TuiApp::cancelled_prompt`,
`crates/squeezy-tui/src/lib.rs:704-721`) but the assistant's mid-stream
work cannot be referenced.

## Finding 3 — EVAL_HARNESS docs document a `wait_for` syntax that does not parse

- Severity: **major (P1)**.
- Rubric dimension: 3 (messaging) — toolchain documentation precision.
- bd id: `squeezy-k1yx`.
- File: `docs/internal/EVAL_HARNESS.md` (`Steps: prompts and actions`
  table) vs. `crates/squeezy-eval/src/scenario.rs:154-161`.

The doc shows the inline form
`wait_for = { kind = "text_contains", text = "compiles" }`. The Rust
enum is declared `#[serde(rename_all = "snake_case")]` with no
`tag = "..."` attribute — externally tagged. The form serde accepts is
`wait_for = { text_contains = { text = "5." } }`.

Impact in this domain: all three wave2-11 scenario files on main used
the documented (wrong) form. Before the fix:

```
squeezy-eval: scenario parse: parsing "…wave2-11-streaming-cancel-openai.toml":
TOML parse error at line 62, column 1
   |
62 | [[steps]]
   | ^^^^^^^^^
invalid value: map, expected map with a single key
```

After applying the externally-tagged form to all three scenarios the
runs go end-to-end. The fix was minimal and is included in this
worktree's diff:

```
- wait_for = { kind = "text_contains", text = "5." }
+ wait_for = { text_contains = { text = "5." } }
```

Either the doc gets corrected to the externally-tagged inline form, or
`WaitFor` gets `#[serde(tag = "kind")]` to match. The doc-form lands
elsewhere in the wave-2 dispatch board, so a `squeezy-eval check` pass
over `crates/squeezy-eval/fixtures/scenarios/` would surface any other
copies before they regress.

## Finding 4 — `input_tokens` semantics diverge between providers

- Severity: **medium (P2)**.
- Rubric dimension: 6 (cross-provider consistency).
- bd id: `squeezy-umo2`.
- File: provider snapshot composition in
  `crates/squeezy-llm/` (Anthropic vs. OpenAI roll-up).

On the same recovery turn, frames.jsonl reports:

- OpenAI turn 2 (`…openai-1780145063183/frames.jsonl` line 2):
  `input_tokens=5743, cached_input_tokens=5632, output_tokens=11`.
  OpenAI convention: `input_tokens` is the **total** prompt the model
  saw; `cached_input_tokens` is the share that was a cache hit.
- Anthropic turn 2 (`…anthropic-1780145100441/frames.jsonl` line 2):
  `input_tokens=10, cached_input_tokens=8457, output_tokens=81`.
  Anthropic convention: `input_tokens` is the **non-cached delta**.

A reader scanning frames.jsonl sees "input=10" for the Anthropic
recovery turn and is misled — it is in fact 8467 input tokens total.
The `output_tokens=81` for a 31-character reply also conflates
reasoning tokens into the same field without a separate
`reasoning_output_tokens` (which is non-null on OpenAI but absent on
Anthropic in this run).

Recommended fix: normalise so `input_tokens` always means the same
thing across providers (the OpenAI convention is more useful for
cost-cap math), or add a derived `input_tokens_total = input + cached`
field that frames.jsonl writers fill consistently.

## Finding 5 — cancelled-turn `cost_display` is empty string

- Severity: **low (P3)**.
- Rubric dimension: 1 (visual clarity).
- bd id: `squeezy-xinl`.
- File: frames writer fill path in
  `crates/squeezy-eval/src/frames.rs` (search for `cost_display`).

Both cancelled frames carry `"cost_display":""`. The non-cancelled turn
2 renders `"$0.0006"`. Empty string vs. `$0.0000` makes the JSON
ambiguous to a reader — empty could mean "frame writer didn't get there"
or "the cost was zero". The doc invariant (`EVAL_HARNESS.md`: "`0` means
no pricing entry") implies the displayed form should be `"$0.0000"`.

## Finding 6 — Portkey provider config absent; wave2-11-portkey aborts before producing a run dir

- Severity: **medium (P2)** — per the wave-2 hard rule, "provider
  config error → medium finding, not abort."
- Rubric dimension: 6 (cross-provider consistency — missing data point).
- bd id: `squeezy-p4kf`.

Invocation:

```
$ source ~/.env.sh && cargo run -p squeezy-eval --quiet -- run \
    crates/squeezy-eval/fixtures/scenarios/wave2-11-streaming-cancel-portkey.toml --no-triage
squeezy-eval: provider: provider is not configured: missing PORTKEY_API_KEY or
  SQUEEZY_PORTKEY_KEY; set the env var or add `[providers.<name>] api_key = "…"`
  to ~/.squeezy/settings.toml or the project-local settings.toml
hint: for an offline run, set `[squeezy] provider = "mock"` in your scenario
  and add a `[mock]` block with scripted `turns`. See docs/internal/EVAL_HARNESS.md.
```

The error message is correct and actionable. The finding is that the
wave-2 dispatch env did not have a Portkey credential available, so the
Qwen cancel surface — explicitly called out in the scenario doc as the
hardest case in the matrix (the "thinks but emits nothing" pattern) —
was not exercised this wave.

## Cross-provider latency summary

Same probe shape, cancel timing measured from `turn_started` to
`turn_cancelled`:

| Provider | Stream start → cancel | Notes |
|---|---|---|
| openai | seq 5 → seq 136 (~2.1 s wall) | 84 reasoning_deltas + 45 assistant_deltas before cancel; chunks ~6 chars |
| anthropic | seq 5 → seq 18 (~4.2 s wall) | 8 reasoning_deltas + 7 assistant_deltas; chunks ~150 chars |

The eval driver's cancel logic itself is fast on both (within 2 ms of
the `5.` substring matching). The user-visible "I see it stop" feel
differs because Anthropic streams in much larger chunks, so the visible
post-cancel buffer is longer ("…fish out of the bo" vs. "…demo.\n5.").
Not a defect — a streaming-shape difference worth documenting in the
TUI's cancel hint copy.

## Scenarios touched

Three fixture files now use the externally-tagged `wait_for` form so
they parse:

- `crates/squeezy-eval/fixtures/scenarios/wave2-11-streaming-cancel-openai.toml`
- `crates/squeezy-eval/fixtures/scenarios/wave2-11-streaming-cancel-anthropic.toml`
- `crates/squeezy-eval/fixtures/scenarios/wave2-11-streaming-cancel-portkey.toml`

No other scenario shape change; the prompts, expectations, and triage
settings are unchanged.

## Out of scope for this scenario

- The TUI's `Ctrl+R` restore (`restore_cancelled_prompt`,
  `crates/squeezy-tui/src/lib.rs:707-721`) is not observable from the
  eval driver — the driver bypasses the TUI. Validating that surface
  needs a `tui_capture` scenario or the squeezy-harness fixture path.
  Documented in the scenario file headers as a known gap.
- The TUI's idle-after-cancel hint copy
  ("Ctrl-R restore last prompt …", `lib.rs:9922`) is also a TUI-only
  surface; the eval driver cannot assert against it.
