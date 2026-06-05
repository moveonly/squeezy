# wave2-06-plan-mode-question-flow

- **Domain:** Wave-2 / 06 plan-mode question flow (`request_user_input`)
- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-06-plan-mode-question-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-06-plan-mode-question-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-06-plan-mode-question-portkey.toml`
- **Run dirs:**
  - OpenAI: `target/eval/wave2-06-plan-mode-question-openai-1780144000066`
  - Anthropic (original, aborted): `target/eval/wave2-06-plan-mode-question-anthropic-1780144060790`
  - Anthropic (bumped `max_output_tokens = 8192`, for cross-provider coverage):
    `target/eval/wave2-06-plan-mode-question-anthropic-bumped-1780144448087`
  - Portkey (aborted): `target/eval/wave2-06-plan-mode-question-portkey-1780144102455`

## Headline

PR #155 wired the warm-taupe `MODE_PURPLE = Rgb(145, 132, 113)` into the
plan-mode question line. The code constant is correct
(`crates/squeezy-tui/src/render/palette.rs:31`) and the wiring is in
place (`crates/squeezy-tui/src/lib.rs:4396`), but the surrounding modal
chrome still mixes a bright-blue ANSI 256 index 33 ("Answer ›" label)
and `Color::White` (non-selected choice labels, luminance 255), both
violating the wave-2 palette rubric. The agent-side
`handle_request_user_input_call` accepts replies that don't satisfy the
question contract: a `choice_value` outside the offered choices
(OpenAI run) and a freeform reply when `allow_freeform = false`
(Anthropic run) both round-trip back as `ok: true`. The Anthropic
scenario's pinned `max_output_tokens = 1024` collides with the user's
`reasoning_effort` setting and aborts the entire turn at the provider
boundary. Portkey aborts before the first prompt because the API key
is unconfigured.

The MODE_PURPLE value itself cannot be **visually** confirmed in this
domain — the wave-1 harness gap (`pump_until_idle` deadlocks on modal
overlays at `crates/squeezy-tui/src/testing.rs:124`) forces
`drive_tui = false`, so the `frames_tui.jsonl` capture is the
"Overlay state" diagnostic dump, not the styled modal area.

## Defects (per finding)

### F1 — choice_value not validated against offered choices

- **Severity:** medium (functionality / cross-model consistency)
- **Provider:** OpenAI (`gpt-5.4-mini`)
- **Rubric dimension:** Functionality
- **File:line:** `crates/squeezy-agent/src/lib.rs:6479-6500`
- **bd ticket:** `squeezy-azkf`
- **Evidence:**
  - `target/eval/wave2-06-plan-mode-question-openai-1780144000066/trace.jsonl`
    seq 187: agent request with `choices = [Simplify=simplify,
    Split=split, Perf=perf]`.
  - seq 188 `action_step.status = "choice:small"` — driver replied
    `"small"`, not a member of the offered values.
  - seq 190 `tool_call_completed.status = Success` with
    `{"action":"choice","choice_value":"small","ok":true}`.
- **Suggested fix:** validate `response.choice_value` against
  `args.choices` values in `handle_request_user_input_call`; on
  mismatch return `ToolStatus::Error` with
  `"choice_value not in offered choices"`.

### F2 — freeform reply accepted when `allow_freeform = false`

- **Severity:** medium (functionality / cross-model consistency)
- **Provider:** Anthropic (`claude-haiku-4-5-20251001`, bumped run)
- **Rubric dimension:** Functionality
- **File:line:** `crates/squeezy-agent/src/lib.rs:6479-6500`
  (and the `Args` parse at `lib.rs:6399-6411`)
- **bd ticket:** `squeezy-ilp5`
- **Evidence:**
  - `target/eval/wave2-06-plan-mode-question-anthropic-bumped-1780144448087/trace.jsonl`
    seq 29: request `{question:"What is the primary goal of this
    refactor?", choices:[...4 items], allow_freeform: omitted →
    false}`.
  - seq 30 `action_step.status =
    "freeform:Yes — focus on the eval driver's request_user_input
    path first."`.
  - seq 31 `tool_call_completed.status = Success` with
    `{"action":"freeform","freeform":"Yes — focus on…","ok":true}`.
- **Suggested fix:** when `args.allow_freeform == false` and
  `response.action == Freeform`, return `ToolStatus::Error` with
  `"freeform not allowed for this question"`.

### F3 — Anthropic thinking budget clamps below provider minimum

- **Severity:** medium (provider config error per wave-2 rule)
- **Provider:** Anthropic (`claude-haiku-4-5-20251001`)
- **Rubric dimension:** Functionality / cross-model consistency
- **File:line:** `crates/squeezy-llm/src/anthropic.rs:139-144`
- **bd ticket:** `squeezy-zh0n`
- **Evidence:**
  - `target/eval/wave2-06-plan-mode-question-anthropic-1780144060790/trace.jsonl`:
    9 events, 1 empty frame, $0.0000. The single trace `turn_failed`
    carries
    `provider request failed: 400 Bad Request: ... thinking.enabled.budget_tokens: Input should be greater than or equal to 1024`.
  - Math: scenario sets `max_output_tokens = 1024`; user settings.toml
    pins `reasoning_effort`; `ReasoningEffort::Low.thinking_budget_tokens()
    = 4096`; `anthropic.rs:140` computes `budget = min(4096, 1024 -
    1) = 1023`; Anthropic rejects.
- **Suggested fix:** either bump `max_tokens` so the clamp doesn't
  collapse below 1024 (preferred — surface a one-line WARN to the
  operator), or omit the `thinking` block entirely when the computed
  budget would be below the provider minimum so the turn still runs
  without reasoning.

### F4 — Answer label uses bright-blue ANSI index 33 inside the warm-taupe modal

- **Severity:** medium (visual clarity — palette guardrail)
- **Provider:** all (TUI-only)
- **Rubric dimension:** Visual clarity
- **File:line:** `crates/squeezy-tui/src/lib.rs:4427-4428`
- **bd ticket:** `squeezy-lbd9`
- **Evidence:** source-level. `Color::Indexed(33)` resolves to
  `Rgb(0, 135, 255)` (luminance 108 — within the <=160 budget, but the
  wave-2 plan reserves the request_user_input modal for `MODE_PURPLE`).
  Bright-blue inside a warm-taupe modal reads as a hyperlink, not a
  focus cue, and breaks the "one semantic colour per modal" rule.
- **Suggested fix:** replace `Color::Indexed(33)` with `MODE_PURPLE`
  (and adjust the inline cursor `Style::default().fg(Color::Black).bg(Color::Indexed(33))`
  at `lib.rs:4428` similarly — `MODE_PURPLE` bg or `QUIET` bg keeps the
  block visible without injecting a foreign colour).

### F5 — non-selected choice labels paint `Color::White` (luminance 255)

- **Severity:** medium (visual clarity — palette guardrail)
- **Provider:** all (TUI-only)
- **Rubric dimension:** Visual clarity
- **File:line:** `crates/squeezy-tui/src/lib.rs:4406` (and the
  freeform `entry_style` at `lib.rs:4426`)
- **bd ticket:** `squeezy-4zsv`
- **Evidence:** wave-2 plan: "Any cell rendering with a luminance > 160
  RGB is a finding". `Color::White = Rgb(255, 255, 255)` → luminance
  255, which exceeds the budget by 95.
- **Suggested fix:** use `muted_fg()` (the tone-aware muted
  foreground at `palette.rs:370`) or a darker mid-tone for
  non-selected labels; reserve a slightly stronger cue (e.g.
  `footer_fg` + `Modifier::BOLD`) for the freeform answer body.

### F6 — modal height underestimates wrapped lines; long labels overflow

- **Severity:** low-medium (cross-model consistency)
- **Provider:** Anthropic (long verbose labels; OpenAI hides this
  regression because it emits short labels)
- **Rubric dimension:** Cross-model consistency / progressive
  disclosure
- **File:line:** `crates/squeezy-tui/src/lib.rs:5113-5118` (and the
  surrounding `approval_menu_height` at `lib.rs:5093-5121`)
- **bd ticket:** `squeezy-xtvg`
- **Evidence:** Anthropic Haiku emitted a 67-char choice label
  ("Improve code structure/maintainability (split modules, reduce
  duplication)") inside the
  `wave2-06-plan-mode-question-anthropic-bumped-1780144448087` run.
  The modal allocates `lines.len() as u16` rows; `Wrap{trim:false}`
  then wraps each long label to two visible rows, which pushes the
  freeform answer box off the allocated area on a 120-wide terminal.
- **Suggested fix:** compute modal height using a textwrap pass
  matching the modal width (use `textwrap::wrap` or a ratatui-aware
  helper) instead of raw line count.

### F7 — Portkey provider unconfigured; whole run aborts before first prompt

- **Severity:** medium (provider config error per wave-2 rule)
- **Provider:** Portkey (`@openrouter/qwen/qwen3.6-35b-a3b`)
- **Rubric dimension:** Cross-model consistency / functionality
- **File:line:** N/A — config gap, not a code defect. Hint surfaced
  by `crates/squeezy-core` via the provider hint in
  `crates/squeezy-eval/src/driver.rs:510`.
- **bd ticket:** `squeezy-m07b`
- **Evidence:**
  - `target/eval/wave2-06-plan-mode-question-portkey-1780144102455`:
    all three jsonl outputs are 0 bytes — the run aborted before the
    driver could emit a single trace event.
  - CLI output: `provider is not configured: missing PORTKEY_API_KEY
    or SQUEEZY_PORTKEY_KEY`.
- **Suggested fix:** either provision a Portkey key for the wave-2
  rotation, **or** emit a structured `provider_unconfigured` finding
  + a minimal `run.json` so the eval batch can continue and the
  missing-key state is observable in CI.

### F8 — `permission_mode = "allow"` in scenario overlay does not cover read/search

- **Severity:** low (scenario-design / harness ergonomics)
- **Provider:** all (manifests on every plan-mode probe that touches
  navigation tools)
- **Rubric dimension:** Functionality
- **File:line:** `crates/squeezy-eval/src/driver.rs:476-485`
- **bd ticket:** `squeezy-lj5c`
- **Evidence:**
  - `target/eval/wave2-06-plan-mode-question-openai-1780144000066/trace.jsonl`
    seqs 103, 105: `approval` for `repo_map` and `grep` both end
    `denied_no_action` despite the scenario setting `permission_mode
    = "allow"`. The eval overlay applies `allow` to edit/shell/web/mcp
    but leaves read and ignored_search at their settings.toml defaults
    (Ask).
  - The auto-denials snowball into six unrelated `approval_unanswered`
    + `denied_tool_call_ux` findings on the OpenAI run and three more
    on the Anthropic bumped run.
- **Suggested fix:** in `apply_overlay`, when `permission_mode` is
  supplied also set `config.permissions.read` and
  `config.permissions.ignored_search`, or document the gap explicitly
  in `EVAL_HARNESS.md` so scenario authors realise navigation tools
  need an explicit allow rule.

## How to re-run

```sh
source ~/.env.sh

cargo build -p squeezy-eval

cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-06-plan-mode-question-openai.toml \
  --no-triage

cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-06-plan-mode-question-anthropic.toml \
  --no-triage

cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-06-plan-mode-question-portkey.toml \
  --no-triage
```

## Cross-provider divergence

- **OpenAI** emits two short questions in the same turn (3 short
  one-word choices, no freeform). Hits F1, F8.
- **Anthropic** (with bumped `max_output_tokens`) emits two questions
  with **very long verbose labels** (40-70 chars) and ignores
  `allow_freeform`; the agent accepts the freeform reply as if it were
  legal. Hits F2, F6, F8. At the pinned `max_output_tokens = 1024` it
  hits F3 and aborts before producing any modal at all.
- **Portkey** never reached the modal — provider config gap (F7).

The wave-1 harness deadlock blocks rendered-frame regression
coverage of every modal that suspends a turn (request_user_input,
MCP elicitation, plan approval); F4 + F5 + F6 are source-level
findings that must be confirmed with a TUI-level test until the
harness gap is closed.

## Known harness limitations carried into this report

- `drive_tui = false` (per wave-1 finding
  `docs/internal/eval-findings/plan-mode-question-styling.md`) →
  no styled-cell capture, only the "Overlay state" diagnostic text
  in `frames_tui.jsonl`.
- The eval driver consumes only one queued `respond_user_input` per
  matching request. Both OpenAI and Anthropic emit two
  `request_user_input` calls per turn, so the second one always
  auto-cancels here. That auto-cancel is harness behaviour, not a
  defect — but it inflates the `tui_overlay_unhandled` finding
  count.
