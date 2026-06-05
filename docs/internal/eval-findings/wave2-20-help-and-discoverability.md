# wave2-20-help-and-discoverability

- **Domain:** Wave-2 #20 help-and-discoverability (per `docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`)
- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-20-help-discoverability-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-20-help-discoverability-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-20-help-discoverability-portkey.toml`
- **Run dirs:**
  - `target/eval/wave2-20-help-discoverability-openai-1780145789796/`
  - `target/eval/wave2-20-help-discoverability-anthropic-1780145851448/`
  - `target/eval/wave2-20-help-discoverability-portkey-1780145891522/` (provider-config error; only empty `trace.jsonl` + `frames.jsonl`)

## Probe shape

Each scenario sends the same two discoverability prompts a new user
would naturally ask after launching squeezy without reading any docs:

1. *"How do I cancel an in-flight model response in squeezy? Answer
   in one short sentence — name the key or command a user would press."*
2. *"Where in the codebase is that cancel handled? Name the file path
   under `crates/`."*

The canonical answer set:
- key: **Esc** or **Ctrl+C** — footer hint at
  `crates/squeezy-tui/src/lib.rs:9910` reads
  `"Ctrl-C/Esc interrupt · Enter queue · ..."`
- dispatch site: `request_turn_interrupt` at
  `crates/squeezy-tui/src/lib.rs:1799`, wired from
  `crates/squeezy-tui/src/lib.rs:1396`
  (`if key.code == KeyCode::Esc && request_turn_interrupt(app)`)
- `SLASH_COMMANDS` (`crates/squeezy-tui/src/input.rs:108`) has no
  `/stop`, `/cancel`, or `/abort` entry — the cancel surface is
  keyboard-only.

## Finding 1 — local help interceptor hijacks "how do I cancel" with a generic agent-topic dump

- **Severity:** medium
- **Rubric:** Functionality + Messaging (the surface does not answer
  the question its name promises; the user is dropped into a TOML
  wall instead of a single-word answer)
- **Beads:** `squeezy-p72m`
- **Suspect:** `crates/squeezy-skills/src/help.rs:100`
  (`SqueezyHelp::answer_for_input`) routes the question into
  `looks_like_squeezy_help_question`
  (`crates/squeezy-skills/src/help.rs:530`) which trips because the
  prompt contains "squeezy" + "how do i" + ends with `?`.
  `best_topic_for_text` (`crates/squeezy-skills/src/help.rs:694`)
  picks the closest match — the `agent` topic
  (`crates/squeezy-skills/src/help.rs:194`) — and emits its summary
  plus a redacted `config inspect` dump for `[agent]` / `[session]`
  / `[tools]` / `[budgets]` / `[tui]`. The actual cancel answer
  (Esc / Ctrl+C) is nowhere in the response, the model is never
  called for that turn, and the only citations are bundled external-doc paths
  (`crates/squeezy-skills/external-docs/AGENT_APPROACH.md`, etc.) that
  themselves do not name the cancel keys.

### What you should see vs. what you see

- Expected: a one-sentence answer naming **Esc** (or **Ctrl+C**).
- Observed: a ~60-line response titled
  `"Squeezy help: agent approach, modes, tools, and local-first workflow"`
  followed by a redacted `[agent]` / `[session]` / `[tools]` /
  `[budgets]` / `[tui]` TOML block and the disclaimer
  `"This answer is limited to local Squeezy docs and config inspect output."`

### Cross-provider evidence (this is shared-code, not a model bug)

Both providers produce the *byte-identical* turn-1 output because the
help interceptor short-circuits the agent loop before any LLM dispatch.

- `target/eval/wave2-20-help-discoverability-openai-1780145789796/frames.jsonl`
  `TurnId(1)`: `assistant_text` opens with the canonical
  `"Squeezy help: agent approach, modes, tools, and local-first workflow"`
  body. `cost_display = "$0.0000"`, `input_tokens = 0`,
  `output_tokens = 0`. The turn completed in ~3 ms
  (`trace.jsonl` `turn_started` seq 4 → `turn_completed` seq 7).
- `target/eval/wave2-20-help-discoverability-anthropic-1780145851448/frames.jsonl`
  `TurnId(1)`: same `assistant_text` prefix, same `$0.0000`, same
  zero-token totals, same ~3 ms turn.

The smoking-gun signal that this is local-handler output, not a
provider answer: `cost_micro_usd = 0` on both runs is impossible
for a real LLM round trip.

### Reproduction

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-20-help-discoverability-openai.toml \
  --no-triage
```

### Suggested fix

Either of the following — both keep the local-help feature alive:

- **Add a `cancel` / `keys` topic** to `TOPICS` in
  `crates/squeezy-skills/src/help.rs:194`. The summary should name
  Esc / Ctrl+C verbatim, cite the footer hint, and link to
  `docs/internal/KEYBINDINGS.md` or a new packaged external-doc keybinding
  topic if one is added.
- **Tighten `looks_like_squeezy_help_question`**
  (`crates/squeezy-skills/src/help.rs:530`) so action questions
  ("how do I X") only intercept when the input also matches a topic
  alias exactly. Today the function accepts "how do i" + the word
  "squeezy" anywhere in the prompt, so any discoverability question
  with the product name in it gets hijacked.

## Finding 2 — openai gpt-5.4-mini cites a wrong file (`squeezy-agent/src/cancel.rs`) for the cancel handler

- **Severity:** medium (cross-provider regression — anthropic gets
  this right on the same probe shape)
- **Rubric:** Functionality + Cross-model consistency
- **Beads:** `squeezy-1hcm`
- **Suspect (root cause):** discoverability help is sparse — there is
  no system-prompt nudge or onboarding-summary content steering the
  model toward the TUI cancel surface
  (`crates/squeezy-tui/src/lib.rs:1799`). Providers that haven't
  recently inspected the repo guess based on filename plausibility,
  and `squeezy-agent/src/cancel.rs` *does* exist (it's an
  `OrCancelExt` futures helper at lines 1-29) but is not the file a
  user touches to cancel a turn.

### What you should see vs. what you see

- Expected: a path under `crates/squeezy-tui/` (specifically
  `src/lib.rs`).
- Observed (openai): `crates/squeezy-agent/src/cancel.rs`.
- Observed (anthropic): `crates/squeezy-tui/` — correct surface,
  though it stops at the crate root.

### Evidence

- `target/eval/wave2-20-help-discoverability-openai-1780145789796/frames.jsonl`
  `TurnId(2)` `assistant_text` ends with:
  `` `crates/squeezy-agent/src/cancel.rs` ``.
  The model dispatched four tool calls (one denied `grep`, one denied
  `definition_search`, one denied `glob`, one allowed `shell` running
  `find crates -name '*.rs'`) and still landed on the wrong file.
  `findings.jsonl` records the resulting
  `expect_final_text_contains` minor finding because
  `squeezy-tui` is missing from the assistant output.
- `target/eval/wave2-20-help-discoverability-anthropic-1780145851448/frames.jsonl`
  `TurnId(2)` `assistant_text`:
  `"...the cancel/interrupt logic would most likely be in:
  **\`crates/squeezy-tui/\`** — specifically in the event loop or
  input handler module..."`. No `expect_final_text_contains` miss for
  this provider.

### Reproduction

Same as Finding 1; the cross-provider divergence is visible in turn 2
of each run.

### Suggested fix

- Add a brief discoverability section to the onboarding summary or
  `crates/squeezy-skills/external-docs/AGENT_APPROACH.md` that
  names `request_turn_interrupt` at
  `crates/squeezy-tui/src/lib.rs:1799` and the Esc / Ctrl+C keys.
  This grounds every provider on the right answer.
- Independently, the openai-side failure is also model behaviour;
  upgrading to a stronger reasoning effort or a larger model would
  likely also fix it, but the fix above generalises across providers.

## Finding 3 — portkey provider config error (medium, not abort)

- **Severity:** medium (per agent dispatch hard rule and
  `EVAL_COVERAGE_PLAN_WAVE2.md` triage rules: provider-config
  error → medium finding, not abort)
- **Rubric:** infrastructure (no defect in product code; gap in test
  env)
- **Beads:** `squeezy-qnbd`

### Symptom

```
squeezy-eval: provider: provider is not configured: missing
PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add
`[providers.<name>] api_key = "…"` to ~/.squeezy/settings.toml or
the project-local settings.toml
```

### Evidence

`target/eval/wave2-20-help-discoverability-portkey-1780145891522/`
contains only empty `trace.jsonl` and `frames.jsonl` — the harness
errored cleanly before any session artifacts were emitted; there is
no `run.json`, no `findings.jsonl`, no `tickets/`.

### Effect on the cross-provider matrix

The portkey leg of wave-2 #20 could not be evaluated; the
two-provider comparison (openai vs. anthropic) still holds and is
the basis of Finding 2 above. Once the portkey key is wired on the
dispatching account, rerun:

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-20-help-discoverability-portkey.toml \
  --no-triage
```

## Why the auto-findings fired but did not name these defects

- `expect_final_text_contains` (minor) fired on the openai run
  because `"squeezy-tui"` did not appear in the final turn — that is
  the surface signal Finding 2 sits on top of, but the rule only
  records the keyword miss, not the off-base citation.
- `approval_unanswered` (major) fired because the openai run's
  exploration tools (`grep`, `definition_search`, `glob`) raised
  approval requests that the scenario does not pre-answer. This is a
  side-effect of `permission_mode = "allow"` not auto-approving the
  graph-backed search tools, not a product defect. It is noise for
  this domain; see the wave-2 `tool-approval-allow-deny` scenarios
  for the canonical probe of that surface.
- `denied_tool_call_ux` (minor) fired for each denied tool call —
  same noise source.
- No rule audits the help-interceptor short-circuit path; Finding 1
  would need a new rule
  (e.g. `local_help_zero_token_turn_intercept`) to surface
  automatically.

## Summary table

| # | Provider | Severity | Dimension | `file:line` | Beads |
|---|---|---|---|---|---|
| 1 | openai + anthropic | medium | Functionality + Messaging | `crates/squeezy-skills/src/help.rs:100`, `:530`, `:194` | `squeezy-p72m` |
| 2 | openai (vs. anthropic) | medium | Functionality + Cross-model consistency | `crates/squeezy-tui/src/lib.rs:1799` (the file the model should have cited) | `squeezy-1hcm` |
| 3 | portkey | medium | infrastructure | n/a (env-config gap) | `squeezy-qnbd` |
