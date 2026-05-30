# wave2-16 status-line-and-cost

Status-line render order, cost segment formatting, and cost-cap warning
surface — probed live against three providers via the eval harness.
Per the wave-2 brief, `max_session_cost_usd_micros` was pinned to
`10_000` micros (~$0.01) on each scenario so a single live turn against
the squeezy repo would deterministically trip the cap.

## Scenarios

- `crates/squeezy-eval/fixtures/scenarios/wave2-16-status-line-cost-openai.toml`
- `crates/squeezy-eval/fixtures/scenarios/wave2-16-status-line-cost-anthropic.toml`
- `crates/squeezy-eval/fixtures/scenarios/wave2-16-status-line-cost-portkey.toml`

## Run directories

- `target/eval/wave2-16-status-line-cost-openai-1780145454368/`
- `target/eval/wave2-16-status-line-cost-anthropic-1780145496665/`
- (portkey: provider-config bail, no run dir — see Finding 3.)

## Per-finding summaries

### Finding 1 — Cost cap reached error lacks next-step guidance

## Severity

medium — messaging rubric: error message must cite a next step (the
wave-2 brief and `EVAL_COVERAGE_PLAN_WAVE2.md` rubric question 3 call
this out).

## What you should see vs. what you see

- Expected: When the session cost cap trips, the user sees a message
  like `session cost cap reached: spent $X of $Y (N%) — raise the cap
  with /config or restart the session`. Both the transcript notice
  and the broker's bailout reason cite a next step.
- Observed: The broker emits `session cost cap reached: spent
  $0.012457 of $0.010000 (124%)` — no next step. The warning notice
  emits `session cost crossed warning threshold: spent $0.0096 of
  $0.01 cap (96%)` — also no next step.

## Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-16-status-line-cost-anthropic.toml \
  --no-triage
```

## Evidence

- `crates/squeezy-agent/src/cost_broker.rs:259-266`: `format_cap_reached_reason`
  format string with no actionable next step.
- `crates/squeezy-tui/src/events.rs:363-371`: CostWarning transcript
  notice format with no actionable next step.
- Run dir `target/eval/wave2-16-status-line-cost-anthropic-1780145496665`,
  `trace.jsonl` seq 27 `turn_failed.error`.
- Beads: `squeezy-zp6e`.

## Suspected cause

Hand-rolled `format!` strings in the broker and the TUI event
adapter were written for the "rare technical event" case rather than
the "user-actionable message" case. The error text needs to be a
one-liner the user can react to without consulting the docs.

---

### Finding 2 — Cost cap trips post-hoc (124% of cap)

## Severity

medium — functionality rubric: a "hard cap" that triggers only after
the over-spend has already been billed is not a hard cap.

## What you should see vs. what you see

- Expected: `max_session_cost_usd_micros = 10_000` means the session
  cannot spend more than $0.01. A pre-flight estimate against the
  upcoming LLM request should short-circuit before the request fires
  when the projected post-call spend would exceed the cap.
- Observed: The cap fires only after the broker sees
  `session_cost_usd_micros >= cap` (the post-billing accounting).
  Reported overrun: `spent $0.012457 of $0.010000 (124%)` — 24% over
  before the broker tripped.

## Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-16-status-line-cost-anthropic.toml \
  --no-triage
```

## Evidence

- `crates/squeezy-agent/src/cost_broker.rs:121`: `if
  self.session_cost_usd_micros >= cap` — post-hoc check.
- Run dir `target/eval/wave2-16-status-line-cost-anthropic-1780145496665`,
  `trace.jsonl` seq 26 `task_state_updated.summary`, seq 27
  `turn_failed.error` (both report 124%).
- `cost.estimated_usd_micros = 4836` after the first OpenAI turn
  (`wave2-16-status-line-cost-openai-1780145454368/run.json`),
  showing the same cap value is well under the post-trip Anthropic
  spend.
- Beads: `squeezy-xt2o`.

## Suspected cause

`CostBroker::is_over_cap` (or equivalent) is consulted after the
provider response lands, not before the request is dispatched. A
pre-flight check would require estimating the next request's cost
using `squeezy_llm::estimate_cost` on the assembled prompt plus a
forecast of the output cost, then refusing to send when
`spent + projected >= cap`.

---

### Finding 3 — Portkey provider config not detected by eval harness

## Severity

medium — cross-model consistency rubric, also a wave-2 hard-rule
(provider config error -> medium finding, not abort).

## What you should see vs. what you see

- Expected: With Portkey configured (the wave-2 plan specifies
  `[providers.portkey] api_key = "..."` in
  `~/.squeezy/settings.toml`), `cargo run -p squeezy-eval -- run
  wave2-16-...-portkey.toml` runs the live scenario. If no key is
  present, the error message clearly distinguishes "no env var" from
  "no settings entry" and steers to the latter.
- Observed: The CLI aborts with `provider is not configured: missing
  PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add
  [providers.<name>] api_key = "…" to ~/.squeezy/settings.toml or
  the project-local settings.toml`. The eval harness either doesn't
  pick up the settings.toml entry, or the operator's settings.toml
  has no portkey key (this run could not investigate per the hard
  rules "no reading settings/env API-key files").

## Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-16-status-line-cost-portkey.toml \
  --no-triage
```

## Evidence

- Reported stderr line above (no run dir produced).
- Cross-provider consequence: the Portkey arm of the wave-2 cost-cap
  comparison cannot run, so we cannot validate that Portkey/OpenRouter
  cost-tracking trips the same cap behaviour as OpenAI + Anthropic.
  Cross-model consistency rubric question 6 is unverifiable.
- Beads: `squeezy-vsge`.

## Suspected cause

Either (a) the settings.toml plumbing for `[providers.portkey]
api_key` is not being threaded into `AppConfig` when the eval harness
constructs the resolved config, or (b) the operator's local
settings.toml does not have a portkey entry. Per the wave-2 rules we
cannot inspect the credentials file to distinguish.

---

### Finding 4 — read_tool_output requires approval under permission_mode=allow

## Severity

medium — functionality. Surfaced as a major auto-finding
(`approval_unanswered`) and a minor `denied_tool_call_ux`.

## What you should see vs. what you see

- Expected: With `permission_mode = "allow"`, internal recovery
  tools like `read_tool_output` (called by the model to recover a
  spilled stdout buffer) should not require approval. The user
  explicitly opted into permissive operation.
- Observed: The Anthropic scenario shell tool produced 32 KB of
  stdout (just above the spill threshold). The model immediately
  called `read_tool_output` to recover the full text. The agent
  raised `ApprovalRequested`, the eval driver auto-denied (no
  matching `approve` action was scripted), and the model received
  `permission_denied: true` with `error: "user denied tool call;
  capability=read target=workspace:* risk=low"`.

## Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-16-status-line-cost-anthropic.toml \
  --no-triage
```

## Evidence

- Run dir `target/eval/wave2-16-status-line-cost-anthropic-1780145496665`,
  `trace.jsonl` seq 20 `approval` for `read_tool_output`, seq 21
  `tool_call_completed status=Denied`.
- `tickets/01-approval-unanswered.{md,json}` and
  `tickets/03-denied-tool-call-ux.{md,json}` in the same run dir.
- Cross-model: only triggered on Anthropic in this probe — the
  OpenAI shell call returned `exit=1` with no stdout (the OpenAI
  prompt used `pub fn` literally and got zero matches), so OpenAI
  never reached the spill path. Portkey could not be tested.
- Beads: `squeezy-bsr0`.

## Suspected cause

`permission_mode = "allow"` is being applied to model-issued tool
calls but `read_tool_output` is being gated by a different
permission policy (likely the per-tool `capability=read
target=workspace:*` policy). Either the policy needs a hole for
`read_tool_output`, or `permission_mode = "allow"` should bypass
per-tool capability checks for recovery affordances the agent
itself surfaces in spill envelopes.
