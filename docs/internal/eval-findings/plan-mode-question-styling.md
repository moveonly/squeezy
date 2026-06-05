# plan-mode-question-styling

- **Area:** TUI plan-mode `request_user_input` modal (PR #154 styling)
- **Scenario:** `crates/squeezy-eval/fixtures/scenarios/plan-mode-question-styling.toml`
- **Run dir:** `target/eval/plan-mode-question-styling-1780101029858/`
- **Severity:** minor (harness gap; no defect found in PR #154 itself — regression guard)
- **Status:** historical snapshot. The scenario passed in this run, and the
  modal pump deadlock below has since been addressed in the TUI test harness;
  keep this file as evidence of the original regression target.

## Summary

Probed the plan-mode question modal end-to-end:

1. Question text round-trips. The mock-issued `request_user_input`
   tool call carries `question = "Which report shape should the
   fixture prefer?"` and both choices verbatim through to the
   `tool_call_queued` event. Sequence 6 in `trace.jsonl`.
2. Selection → response is index-correct. The driver action
   `respond_user_input { choice = "narrative" }` (the value of
   `choices[1]`) is the value the agent's `tool_call_completed`
   reports back: `{"action":"choice","choice_value":"narrative"}`
   at sequence 9. This pins the off-by-one defense in
   `crates/squeezy-tui/src/input.rs:941` (`choices.get(selection_index)`)
   and the upstream selection in
   `crates/squeezy-eval/src/driver.rs:1979`.
3. Esc/cancel path: not exercised in this run because the driver's
   `respond_user_input` action consumes the request immediately; the
   `RequestUserInputResponse::cancelled` branch in
   `crates/squeezy-tui/src/input.rs:950` is covered by the existing
   `lib_tests.rs:228` unit test.
4. Freeform Enter+typing path: not exercised here (same reason).
   Covered by `lib_tests.rs:186` and `lib_tests.rs:4048` at unit
   level; eval-level coverage is blocked by the harness gap below.

The styling rules introduced by PR #154 —

  - question line: `MODE_PURPLE + Modifier::BOLD`
  - choice label: `Color::White` (no bold)
  - "Answer ›" label: `Color::Indexed(33)` (no bold)

are visible in `crates/squeezy-tui/src/lib.rs:4351-4426` and behave
as documented; no defect was reproduced.

## Repro

```sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/plan-mode-question-styling.toml \
  --no-triage
```

Result: 14 trace events, 1 frame, 0 findings, $0.0000 cost.

## Adjacent gap (worth a follow-up)

The natural shape for this fixture — `[tui_capture] drive_tui = true`,
mock turn 1 emits `request_user_input`, send `Down`+`Enter`, assert
`tui_frame_contains` on the rendered modal — currently deadlocks
`TuiHarness::pump_until_idle`. When the agent's
`handle_request_user_input_call` (`crates/squeezy-agent/src/lib.rs:6473`)
awaits the response oneshot, the harness keeps `app.turn_rx = Some(...)`
(`crates/squeezy-tui/src/events.rs:302-320` does *not* clear it on
`RequestUserInputRequested`). `pump_until_idle`'s exit condition
(`crates/squeezy-tui/src/testing.rs:124`) requires `turn_rx.is_none()`,
so it loops until its 180 s deadline and the scenario aborts with
`harness pump: agent error: pump_until_idle: did not reach idle within 180s`.

Suggested fixes (not applied — task forbids touching production code):

  - **Either:** `TuiHarness` grows a `pump_until_blocked` variant that
    additionally returns when `app.pending_request_user_input.is_some()`
    or `app.pending_mcp_elicitation.is_some()`. Send-key code then
    works while the modal is open.
  - **Or:** the eval driver routes mid-modal actions through a
    bypass that doesn't re-enter `pump_until_idle` before injecting
    keys.

This gap blocks rendered-frame regression coverage of every modal
that suspends a turn (`request_user_input`, MCP elicitation, plan
approval). Recommend filing as `tui-harness-modal-pump-deadlock`.

## Evidence

- `target/eval/plan-mode-question-styling-1780101029858/trace.jsonl`
  sequences 6, 7, 9 (request payload, action_step, completed).
- `target/eval/plan-mode-question-styling-1780101029858/run.json`
  (findings: 0, frames: 1, trace_events: 14).

## Suspected file:line

- No defect in PR #154 itself.
- Harness gap: `crates/squeezy-tui/src/testing.rs:124`
  (`pump_until_idle` exit condition).
- Adjacent: `crates/squeezy-tui/src/events.rs:302-320`
  (`RequestUserInputRequested` handler keeps `turn_rx = Some`).
