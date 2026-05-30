# approval-deny-shell — eval finding report

- **Scenario:** `crates/squeezy-eval/fixtures/scenarios/approval-deny-shell.toml`
- **Run directory:** `target/eval/approval-deny-shell-1780100509235`
- **Provider:** `mock` (offline, zero cost)
- **Status:** **no defect found — regression guard**

## Probe

`permission_mode = "ask"`. The mock issues a single-turn response with
`tool_calls = [{ name = "shell", arguments = { command = "echo hi" } }]`.
A `deny` action with `match.tool = "shell"` is pre-queued so the
driver's `decide_approval` returns
`ToolApprovalDecision::Denied` instead of the default
`denied_no_action` fallback.

Two invariants asserted:

1. The denied tool MUST NOT execute. There must be no
   `tool_call_started` event for the shell call, and the per-turn
   frame must not list shell as a fired (status-bearing) call.
2. There must be no second approval popup for the same call. The
   `approval_unanswered` rule must stay silent (it would fire on a
   `denied_no_action` decision, which is the harness signal that no
   queued action matched).

## Result

`findings.jsonl` is empty. Trace evidence
(`target/eval/approval-deny-shell-1780100509235/trace.jsonl`):

- `seq=7 kind=approval tool=shell decision="denied:regression guard: ..."` —
  the queued deny matched and consumed the approval slot. No
  `denied_no_action` sentinel.
- `seq=9 kind=tool_call_completed status="Denied" tool_name="shell"`
  with `permission_denied: true` and a guidance string steering the
  model away from a retry.
- No `tool_call_started` event ever fires for the shell call. The
  `ToolApprovalDecision::Denied` branch at
  `crates/squeezy-agent/src/lib.rs:10185` maps to
  `ApprovalDecision::Denied(...)` and short-circuits dispatch before
  the executor would emit `ToolCallStarted`.
- Frame 1 (`frames.jsonl`) shows `tool_calls: []` (no started calls)
  alongside `queued_tool_calls: [{ name: "shell", status: null }]`,
  matching the "queued but never dispatched" shape we expect on a
  pre-execution denial.
- Frame 1 `finish: "completed"` — the agent fed the
  `PermissionDenied` result back to the model and the turn closed
  normally. There is no second `approval` event in the trace, so the
  same call_id (`mock-0`) was not re-asked.

The `max_tool_calls = 1` assertion (counting the one queued shell
call) passes, confirming no duplicate approval re-issued the same
shell call.

## Notes on the cancelled-prompt sub-claim

The probe brief also asks whether the triggering prompt is stashed
into `cancelled_prompt` so Ctrl+R restores it after a deny. That
behavior lives entirely inside the TUI
(`crates/squeezy-tui/src/lib.rs`, see `restore_cancelled_prompt`,
`pending_approval`, `cancelled_prompt`) and is not exercised by the
headless eval driver — the eval harness consumes
`AgentEvent::ApprovalRequested` directly via `decide_approval` in
`crates/squeezy-eval/src/driver.rs:2030`, never touching the TUI
state machine. The pieces this scenario can verify (agent-side
denial → no `tool_call_started`, no duplicate approval) all hold.
The Ctrl+R restore path needs a TUI-level fixture
(`squeezy-tui`'s integration tests / `TestBackend`) to be exercised
end-to-end; out of scope for this offline eval probe.

## How to re-run

```sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/approval-deny-shell.toml \
  --no-triage
```

Expected: `trace: 24 events  frames: 2  tickets: 0  cost: $0.0000`,
empty `findings.jsonl`.

## Regression signals to watch

A future regression in the approve/deny path would surface here as one
of:

- `approval_unanswered` finding fires — the queued deny stopped
  matching (e.g., tool name renamed without updating the filter, or
  `decide_approval` regressed to fall through).
- A `tool_call_started` event for `shell` appears between the
  `approval` and `tool_call_completed` events — the denial no longer
  short-circuits dispatch.
- A second `approval` event for the same `call_id` appears — duplicate
  approval popup.
- Frame 1 reports `tool_calls: [{ name: "shell", status: "success" }]`
  — the denied call somehow ran.
