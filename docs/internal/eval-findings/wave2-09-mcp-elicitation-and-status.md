# wave2-09 — mcp-elicitation-and-status

Status: historical snapshot from 2026-05-30. Three live scenarios executed
(openai, anthropic); Portkey errored at provider-config resolution. Both
completed runs captured a harness gap that has since been narrowed: current
`squeezy-eval` supports `[mcp.servers]`, `respond_elicitation`,
`inject_mcp_elicitation`, and `unfired_action` findings. Keep this report as
the captured run evidence, not as the current MCP scenario API.

## Run directories

- openai     : `target/eval/wave2-09-mcp-elicitation-openai-1780143986201`
- anthropic  : `target/eval/wave2-09-mcp-elicitation-anthropic-1780144058481`
- portkey    : *(no run dir — provider unconfigured; see defect 3)*

Cross-provider diff is two-sided only. Both completed runs share the
same end-state shape:
`trace_events = 6`, `frames = 0`, `findings = 0`, and a terminal
`action_step.status = "unfired_no_trigger"` on the queued
`respond_elicitation` step (wave-1 witness pattern intact).

## Defects

### 1. anthropic: thinking budget clamp violates API minimum (1024)

- **Provider:** anthropic / `claude-haiku-4-5-20251001`
- **Severity:** major (P1) — provider rejects request with 400.
- **Rubric dimension:** Functionality / Cross-model consistency.
- **Headline:** When `max_output_tokens < 1024`, squeezy still emits a
  `thinking` block whose `budget_tokens = min(thinking_budget_tokens,
  max_tokens - 1)`. With `max_output_tokens = 512` and
  `reasoning_effort = Low` (4096 tokens) the budget clamps to **511**,
  which is below Anthropic's hard min of 1024. The API returns:
  `thinking.enabled.budget_tokens: Input should be greater than or
  equal to 1024` and the turn aborts with no assistant text and no
  TUI frame. The user sees the raw provider 400 — no actionable
  remediation hint.
- **File:line:**
  `crates/squeezy-llm/src/anthropic.rs:139-144`
- **Evidence:** trace.jsonl seq 1 — `status="provider request
  failed: 400 Bad Request: ...
  thinking.enabled.budget_tokens: Input should be greater than or
  equal to 1024 ..."`. `frames.jsonl` empty.
- **Beads:** `squeezy-71u`

### 2. eval: historical MCP elicitation harness gap

- **Provider:** all (harness gap, surfaces identically on openai +
  anthropic; would also surface on portkey if configured).
- **Severity:** medium (P2). Carries the wave-1
  `mcp-elicitation-form` finding forward — that find documented the
  gap but never opened a Beads ticket.
- **Rubric dimension:** Functionality (coverage hole).
- **Headline:** At the time of the captured run, the queued
  `respond_elicitation` action in the wave2-09 scenarios terminated as
  `action_step.status = "unfired_no_trigger"` in `trace.jsonl`, so the
  MCP modal layer (`format_mcp_elicitation_menu_lines`,
  `pending_mcp_elicitation`), the elicitation policy gate, and the
  response-write path were unreachable from this scenario shape.
- **File:line:**
  - historical: `crates/squeezy-eval/src/scenario.rs` did not yet expose
    the current MCP/injected-elicitation surface
  - `crates/squeezy-eval/src/driver.rs:1413` (`McpElicitationRequested` arm)
  - `crates/squeezy-eval/src/driver.rs:1928` (`decide_elicitation`)
  - `crates/squeezy-mcp/src/lib.rs:300` (`set_elicitation_handler`)
  - `crates/squeezy-agent/src/lib.rs:6266` (`install_mcp_elicitation_handler`)
- **Evidence:** trace.jsonl seq 5 in both runs — `"action":
  {"action":"respond_elicitation",...},"status":"unfired_no_trigger"`.
  `findings.jsonl` empty — current code has an `unfired_action` rule for
  this class.
- **Beads:** `squeezy-y5i`

### 3. eval/portkey: provider config missing — scenario aborts before any capture

- **Provider:** portkey / `@openrouter/qwen/qwen3.6-35b-a3b`
- **Severity:** medium (P2) per wave-2 `Hard rules`
  ("Provider config error -> medium finding").
- **Rubric dimension:** Functionality / Cross-model consistency.
- **Headline:** The portkey run never produced an artifact directory.
  squeezy-eval emitted:
  `provider is not configured: missing PORTKEY_API_KEY or
  SQUEEZY_PORTKEY_KEY; set the env var or add `[providers.<name>]
  api_key = "…"` to ~/.squeezy/settings.toml or the project-local
  settings.toml`.
  Without the third run, cross-provider regression diffing for this
  domain is two-sided only — the most useful comparison shape (the
  Qwen `tool_choice = "required"` + "no tool" misalignment trap baked
  into the scenario at line 79–82) is unreachable.
- **File:line:** scenario:
  `crates/squeezy-eval/fixtures/scenarios/wave2-09-mcp-elicitation-portkey.toml:42`
  (provider declaration); error emitted by squeezy-eval CLI before
  any run dir exists.
- **Evidence:** stderr from `cargo run -p squeezy-eval -- run
  crates/squeezy-eval/fixtures/scenarios/wave2-09-mcp-elicitation-portkey.toml
  --no-triage`. No `target/eval/wave2-09-mcp-elicitation-portkey-*/`
  directory produced.
- **Beads:** `squeezy-bcz`

## Observations that did **not** become defects

- **MCP status snapshot palette** — `format_mcp_status_snapshot`
  (`crates/squeezy-tui/src/lib.rs:9955`) and the "none" branch
  (`format_mcp_status`, line 9948) were reviewed against the wave-2
  palette guardrails. No bright accents in scope; the string is
  routed through the QUIET (DarkGray) status-line cell. No palette
  violation observed in source. Note: this was a source-review only
  because no TUI frame was captured (both turns aborted, see defects
  1 and 2).
- **MCP "tools" preamble** — assistant text was never produced
  (`frames.jsonl` empty in both runs), so the recited preamble could
  not be inspected. The scenarios are correctly structured to surface
  this on a successful run; defect 1 must clear first.
- **OpenAI 512-token cap** — the openai run terminated with
  `agent error: model response stopped after max_tokens before
  completing; lower reasoning_effort, raise the provider's
  max_output_tokens, or run /compact and retry`. That is a
  well-formed actionable error, not a defect; it is a scenario-side
  config tightness (the same `max_output_tokens = 512` that exposes
  defect 1 on anthropic). Raising the cap in a follow-up scenario
  edit will unblock the preamble probe.

## Re-run prerequisites

To complete this domain's coverage in a follow-up:

1. Land `squeezy-71u` (anthropic budget clamp), or raise the
   scenario's `max_output_tokens` to >= 1025 + a safety margin.
2. Export `PORTKEY_API_KEY` (or land `squeezy-bcz` via
   settings.toml).
3. For current modal-only coverage, use `inject_mcp_elicitation` with
   `[tui_capture] drive_tui = true`; for transport coverage, declare a
   fake server under `[mcp.servers]`.
