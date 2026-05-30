# mcp-elicitation-form: harness cannot drive an MCP `Form` elicitation offline — needs a `mock_mcp` driver

## Severity

medium — the user-facing modal (styling, multiline input, Esc cancel) is unreachable from the eval harness today, so any visual or input regression in `format_mcp_elicitation_menu_lines` / `pending_mcp_elicitation` handling slips past every scenario-driven probe. It is not data-loss, but it is a hole in the post-#154 styling regression net for one of the more sensitive (user-trust, agent-side privilege) surfaces.

## What you should see vs. what you see

- Expected: a `mock` or live scenario can stand up a fake MCP server, fire a `Form` elicitation, drive `send_key` actions into the response field, assert that the question renders in violet+bold, and confirm `Esc` cancels without leaking the elicitation into the model transcript.
- Observed: the scenario schema has no `[mcp.servers]` / `mock_mcp` block; the only way to populate `app.pending_mcp_elicitation` is for a real MCP server registered against `McpClientRegistry::set_elicitation_handler` to invoke the host-side handler installed by `install_mcp_elicitation_handler`. The eval `MockProvider` does not register an MCP server, so the modal never appears and `send_key`/`tui_frame_contains` assertions never have anything to bind to. The scripted `respond_elicitation` action queued in the scenario surfaces this as a `status="unfired_no_trigger"` `action_step` in `trace.jsonl` — a clean, machine-readable witness of the gap.

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
- `findings.jsonl`: empty — there is no auto-finding rule for "scripted elicitation never fired", which is itself a gap a future `unfired_action` rule could close.

## Suspected cause

This is a harness limitation, not a product defect. Three converging gaps:

- `crates/squeezy-eval/src/scenario.rs` — no `mcp` / `mock_mcp` field on `Scenario`, so authors cannot declare an in-process fake server. The reply-side surface (`Action::RespondElicitation` ~line 232; `ElicitationMatch` ~line 335; `ElicitationDecision` ~line 347) is already wired and waiting.
- `crates/squeezy-eval/src/driver.rs` — `decide_elicitation` (~line 1928) and the `AgentEvent::McpElicitationRequested` arm (~line 1413) consume the reply queue. Both run unchanged the day a driver lands; the missing piece is upstream — there is no scenario-author-controlled way to inject an `McpElicitationRequest` into the agent.
- `crates/squeezy-mcp/src/lib.rs` — the `elicitation_handler` slot (`set_elicitation_handler` ~line 300, called from `install_mcp_elicitation_handler` in `crates/squeezy-agent/src/lib.rs` ~line 6266) only fires from a real MCP server's call path. A `tests/fake-server`-style in-process MCP that the eval driver could register would let the modal surface end-to-end. Alternative: expose a `TuiHarness::push_pending_mcp_elicitation(request)` test hook in `crates/squeezy-tui/src/testing.rs` so scenarios can poke the TUI state directly without round-tripping the MCP transport.

Recommended fix-path: add an optional `[mcp.mock]` section to `Scenario` that takes a `Vec<McpElicitationRequest>` and a trigger (`on_tool="..."` style) and inject those at the same site `install_mcp_elicitation_handler` runs. That keeps the agent code path real and reuses the existing reply queue verbatim. As a smaller fallback, add a `Action::InjectMcpElicitation` action that bypasses MCP entirely and writes straight into `app.pending_mcp_elicitation` via a new `TuiHarness` helper — cheaper to land, narrower coverage (modal layer only, not the elicitation policy gate).
