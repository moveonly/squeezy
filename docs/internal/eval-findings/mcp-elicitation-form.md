# mcp-elicitation-form: historical MCP elicitation harness gap

Status: historical finding. The eval schema now has scenario MCP server
configuration, `respond_elicitation`, `inject_mcp_elicitation`, and an
`unfired_action` finding rule; keep this report as evidence of the original
gap, not as the current harness capability statement.
Use `docs/internal/EVAL_HARNESS.md` and `crates/squeezy-eval/src/scenario.rs`
for the current MCP scenario API.

## Severity

Historical severity: medium. At the time of this run, the user-facing modal
(styling, multiline input, Esc cancel) was unreachable from the eval harness,
so visual or input regressions in `format_mcp_elicitation_menu_lines` /
`pending_mcp_elicitation` handling slipped past scenario-driven probes.

## What you should see vs. what you see

- Expected: a `mock` or live scenario can stand up a fake MCP server, fire a `Form` elicitation, drive `send_key` actions into the response field, assert that the question renders in violet+bold, and confirm `Esc` cancels without leaking the elicitation into the model transcript.
- Observed then: the scenario schema did not have the current
  `[mcp.servers]` / injected-elicitation surface. The eval `MockProvider`
  did not register an MCP server, so the modal never appeared and
  `send_key`/`tui_frame_contains` assertions had nothing to bind to. The
  scripted `respond_elicitation` action queued in the scenario surfaced this
  as a `status="unfired_no_trigger"` `action_step` in `trace.jsonl` — a
  clean, machine-readable witness of the historical gap.

## Reproducer

```sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/mcp-elicitation-form.toml \
  --no-triage
```

Sample run directory:

    target/eval/mcp-elicitation-form-1780100568035

Read `trace.jsonl` and confirm the `respond_elicitation` step's terminal `status` is `unfired_no_trigger` — that is the evidence that no `McpElicitationRequested` event was emitted during the run.

## Evidence

- `trace.jsonl` seq 5: `"action":{"action":"respond_elicitation",...},"status":"unfired_no_trigger"` — driver never got an elicitation event to bind the queued reply to.
- `run.json` totals: `"frames": 0` (no assistant turn ever held an MCP modal up; the mock turn returns inline).
- `findings.jsonl`: empty — at the time there was no auto-finding rule for
  "scripted elicitation never fired"; current code has `unfired_action`.

## Suspected cause

This is a harness limitation, not a product defect. Three converging gaps:

- Historical source note: `crates/squeezy-eval/src/scenario.rs` did not yet
  expose the current MCP server or injected-elicitation API.
- `crates/squeezy-eval/src/driver.rs` — `decide_elicitation` and the
  `AgentEvent::McpElicitationRequested` arm consumed the reply queue.
- `crates/squeezy-mcp/src/lib.rs` — the `elicitation_handler` slot (`set_elicitation_handler` ~line 300, called from `install_mcp_elicitation_handler` in `crates/squeezy-agent/src/lib.rs` ~line 6266) only fires from a real MCP server's call path. A `tests/fake-server`-style in-process MCP that the eval driver could register would let the modal surface end-to-end. Alternative: expose a `TuiHarness::push_pending_mcp_elicitation(request)` test hook in `crates/squeezy-tui/src/testing.rs` so scenarios can poke the TUI state directly without round-tripping the MCP transport.

The smaller fallback described here is the path that exists now:
`Action::InjectMcpElicitation` can exercise the modal layer directly, while
`[mcp.servers]` covers transport-backed fake-server scenarios.
