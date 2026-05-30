# prompt-queue-drain

- **Scenario:** `crates/squeezy-eval/fixtures/scenarios/prompt-queue-drain.toml`
- **Area:** `crates/squeezy-tui/src/prompt_queue.rs`, `crates/squeezy-tui/src/lib.rs` (`auto_drain_queue`, `start_user_turn`, `restore_cancelled_prompt`, Enter handler)
- **Provider:** `mock` (3 scripted turns: `alpha-one`, `beta-two`, `gamma-three`)
- **Most recent run:** `target/eval/prompt-queue-drain-1780100923475`

## Outcome

**No defect found in the prompt-queue drain code itself — landed as a
regression guard.** All four scenario assertions pass: each scripted
mock turn (`alpha-one`, `beta-two`, `gamma-three`) appears in the
rendered TUI frame after its Enter, and the final transcript still
carries a `message` entry, so the queue is not silently dropping the
third prompt under the harness's serial drive path.

## Probe shape

1. `send_keys = ["a", "Enter"]` — type prompt 1, submit. The mock
   provider streams turn 1 (`alpha-one`).
2. Assert `tui_frame_contains "alpha-one"`.
3. `send_keys = ["b", "Shift+Enter"]` — type prompt 2. Shift+Enter
   currently falls through to the Enter handler (no Shift+Enter
   newline binding in the composer), so this submits.
4. Assert `tui_frame_contains "beta-two"`.
5. `send_key = "Esc"` — interrupt path. Under the harness's drive
   model this fires after turn 2 has already drained, so it is a
   no-op in practice (see harness limitation below).
6. `send_keys = ["c", "Enter"]` — prompt 3 fires turn 3.
7. Assert `tui_frame_contains "gamma-three"`.
8. `send_key = "Ctrl+R"` — would restore the last cancelled prompt
   into the composer. With no real cancellation observable from the
   harness, this is a no-op in practice. Assertion uses
   `tui_transcript_entry` to confirm the third user message still
   landed.

## Side findings surfaced while building the scenario

These are not defects in the prompt-queue code per se; they are
harness gaps that prevent this scenario from exercising the
mid-turn-cancel pathway the brief originally described. They are
recorded here so triage can decide whether to land them as separate
tickets.

### 1. `TuiHarness::send_key` serializes via `pump_until_idle`

**File:** `crates/squeezy-tui/src/testing.rs:141`

`send_key` calls `pump_until_idle` both before and after the key is
delivered. `pump_until_idle` only returns when `turn_rx.is_none()`
AND `prompt_queue.is_empty()`. As a consequence, every `Enter` that
starts a turn is followed by a full drain of that turn before the
next key arrives. The Enter handler's mid-turn queue arm
(`app.turn_rx.is_some()` ⇒ push onto `prompt_queue`, status =
`queued (N)`) is unreachable from any scenario that builds prompts
via `send_keys`.

**Severity:** minor (harness expressiveness gap, not a product bug)

**Suggested follow-up:** either expose a `send_key_no_pump` /
`send_key_eager` mode on `TuiHarness`, or a `pump_for(Duration)`
that drains for a bounded window without waiting for idle, so
scenarios can interleave keypresses against a still-running mock
turn (e.g. by configuring `delay_ms` deltas).

### 2. `Action::CancelTurn` is a no-op under `drive_tui = true`

**File:** `crates/squeezy-eval/src/driver.rs:678` (CancelTurn arm),
`crates/squeezy-eval/src/driver.rs:1224` (sole write to
`last_cancel`)

`Action::CancelTurn` reads `self.last_cancel`, which is only written
by `run_prompt` (the non-harness prompt path). When `drive_tui =
true`, scenario prompts route through `run_prompt_through_harness`
which never touches `last_cancel`. The cancel action records
`no_turn_to_cancel` instead of cancelling the live TUI turn. This
is the structural reason the brief's mid-prompt-2 cancel cannot be
expressed via `cancel_turn`; the scenario uses `send_key = "Esc"`
as a workaround (which itself runs into limitation #1).

**Severity:** minor (silent fallthrough — eval users discovering
this read `no_turn_to_cancel` and wonder why)

**Suggested follow-up:** when `drive_tui = true`, route
`Action::CancelTurn` through `harness.send_key(Esc)` (or expose
`harness.request_turn_interrupt()`) so cancellation hits the same
path the TUI uses.

### 3. Harness-driven prompts emit zero turn-level trace events

**File:** `crates/squeezy-eval/src/driver.rs:1052`
(`run_prompt_through_harness`)

`run_prompt_through_harness` records one synthetic `action_step` per
prompt and otherwise emits no `UserMessage` / `TurnStarted` /
`AssistantDelta` / `TurnCompleted` events. The rendered frame is
visible to `tui_frame_contains`, but the per-turn `frames.jsonl`
record, cost accounting, and all the rule engine's regression
heuristics (`duplicate_tool_call`, `stop_with_intent_text_no_tool_call`,
`expect_input_tokens`, etc.) silently never fire for harness-driven
scenarios because they have no per-turn frame to evaluate.

**Severity:** moderate (observability — auto-findings can't trip on
TUI-driven scenarios, which defeats one of the main reasons to run
the harness end-to-end)

**Suggested follow-up:** drain the `app.turn_rx` channel into the
capture pipeline the same way `run_prompt` does — share the
event-loop body, or subscribe to a tee of agent events from inside
the harness.

## Bundle

This is a regression-guard scenario; the run does not currently
emit `tickets/` (no findings fire, triage is disabled). The
side-findings above are out-of-scope for the bundled-evidence
narrative and should be filed as their own tickets if accepted.
