# wave2-12 prompt-queue-and-drain

- **Domain:** 12 / prompt-queue-and-drain
- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-12-prompt-queue-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-12-prompt-queue-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-12-prompt-queue-portkey.toml`
- **Run directories:**
  - OpenAI: `target/eval/wave2-12-prompt-queue-openai-1780144933939`
  - Anthropic: `target/eval/wave2-12-prompt-queue-anthropic-1780145030376`
  - Portkey: provider-config error before any run dir was created (see finding 4)
- **Area:** `crates/squeezy-tui/src/prompt_queue.rs`, `crates/squeezy-tui/src/lib.rs` (`auto_drain_queue`, `start_user_turn`, queue arm at lib.rs:1509-1515 / lib.rs:2061-2064, drain at lib.rs:566-579), `crates/squeezy-llm/src/anthropic.rs` (thinking budget), `crates/squeezy-tui/src/events.rs:448` (`format_error_status` surface)

## Outcome

Three product defects + one operator-side provider-config gap.

The wave-2 prompt-queue probe was authored to drive the live `TuiApp`
via `TuiHarness` and exercise the Enter-handler queue arm
(`turn_rx.is_some() => push onto prompt_queue`) by alternating Enter /
Shift+Enter while a turn is mid-stream. Wave-1's harness gap #1
(`TuiHarness::send_key` calls `pump_until_idle` on both sides of the
keystroke, draining the turn before the next Enter arrives) reproduces
verbatim against live OpenAI streams: the OpenAI run starts and
finishes three turns in 1.2 s + 1.0 s + 1.0 s with status="ready"
between each keystroke, so the queue arm is never hit and no
`queued (N)` chrome surfaces. The Anthropic run never reached even
the first model token because squeezy emits a `thinking.enabled`
block with `budget_tokens = 255` when `max_output_tokens = 256`,
which Anthropic's API rejects with 400.

The product defects below are visible from static analysis +
trace.jsonl + the rendered status line; they survive the wave-1
harness gap because they fire on the squeezed-down code path the
harness *can* reach.

## Findings

### 1. Anthropic thinking.budget_tokens falls below the 1024 floor

- **Provider:** anthropic
- **Severity:** major
- **Dimension:** Functionality, Cross-model consistency
- **File:** `crates/squeezy-llm/src/anthropic.rs:140`
- **Ticket:** `squeezy-xjh7`

`crates/squeezy-llm/src/anthropic.rs:139-144` computes the
thinking-block budget as

```rust
let budget = u64::from(effort.thinking_budget_tokens())
    .min(max_tokens.saturating_sub(1));
body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
```

`effort.thinking_budget_tokens()` is at minimum `4_096` (`Low`,
`crates/squeezy-core/src/lib.rs:2319-2326`). With `max_output_tokens =
256` (chosen so the wave-2 probe keeps cost trivial) the
`min` clamps the value down to `255`, well below Anthropic's API
floor of `1024`. The provider responds:

```
400 Bad Request: {"type":"error","error":{"type":"invalid_request_error",
  "message":"thinking.enabled.budget_tokens: Input should be greater than or equal to 1024"},
  "request_id":"req_011CbYmqWtAUtMpWMuCNYwTS"}
```

Evidence: `target/eval/wave2-12-prompt-queue-anthropic-1780145030376/trace.jsonl`
sequences 1, 5, and 9 all surface the same 400 — every send_keys
submission lands in the failure arm. No model token ever reaches the
TUI. Cross-provider consistency: OpenAI and (unchecked here)
Portkey do not have this floor.

Fix options: clamp `budget` upward to `1024` and gate
`thinking.enabled` on `max_tokens >= 1024 + N`; or skip
`thinking.enabled` whenever `effort.thinking_budget_tokens()` exceeds
`max_tokens - 1` (the squeezed value carries no signal anyway); or
raise `max_output_tokens` automatically when `reasoning_effort` is
set on Anthropic.

### 2. Anthropic 400 surfaces as raw JSON in the TUI status line

- **Provider:** anthropic (any 4xx with a JSON envelope)
- **Severity:** major
- **Dimension:** Messaging
- **File:** `crates/squeezy-tui/src/events.rs:448` (calls
  `format_error_status` in `crates/squeezy-tui/src/lib.rs`)
- **Ticket:** `squeezy-woxv`

When finding #1 fires, the status line renders the entire raw
provider payload verbatim:

```
provider request failed: 400 Bad Request: {"type":"error",
"error":{"type":"invalid_request_error","message":"thinking.enabled.budget_tokens:
Input should be greater than or equal to 1024"},"request_id":"req_011CbYmqWtAUtMpWMuCNYwTS"};
retry or check provider/network status
```

This violates the wave-2 Messaging rubric: error strings must be
concrete, actionable, and free of jargon, with a next step. The
current string is a JSON envelope that wraps off a 120-column TUI and
buries the actionable signal (`budget_tokens must be >= 1024`)
inside the payload.

Evidence: `target/eval/wave2-12-prompt-queue-anthropic-1780145030376/trace.jsonl`
sequence 1 captures the status text. The same text re-fires on every
Anthropic 4xx (the path runs through `AgentEvent::Failed`).

Fix: parse the Anthropic error envelope (`error.type`, `error.message`,
top-level `request_id`) and render a short human line, e.g.:
"Anthropic rejected the request (invalid_request_error):
thinking.enabled.budget_tokens must be >= 1024. Lower reasoning_effort
or raise max_output_tokens. request_id req_011…". Full payload stays
in `trace.jsonl` for triage.

### 3. Queue-overlay non-selected rows render Color::White

- **Provider:** all (palette is provider-independent)
- **Severity:** major
- **Dimension:** Visual clarity
- **File:** `crates/squeezy-tui/src/prompt_queue.rs:167`
- **Ticket:** `squeezy-mfw6`

`prompt_queue::render_lines` styles non-selected queued-prompt rows
with `Style::default().fg(Color::White)`:

```rust
let style = if is_selected {
    Style::default().fg(GOLD).add_modifier(Modifier::BOLD)
} else {
    Style::default().fg(Color::White)        // <-- luminance 255
};
```

`Color::White` resolves to RGB (255, 255, 255), luminance 255 — well
above the wave-2 dark-tone ceiling of 160 (`EVAL_COVERAGE_PLAN_WAVE2.md`,
"Palette guardrails"). The wave-2 luminance audit:

| Token | RGB | Luminance | Pass? |
|---|---|---:|:---:|
| `BUTTON_FG = Indexed(33)` | (0, 135, 255) | 108.3 | yes |
| `GOLD` | (184, 124, 38) | 132.1 | yes |
| `Color::White` (this finding) | (255, 255, 255) | 255.0 | **no** |
| `QUIET` (DarkGray) | (80, 80, 80) | 80 | yes |

Because the harness-driven runs never reach the queue-arm code path
(wave-1 harness gap #1), the offending cells do not appear in this
domain's rendered frames. The finding is statically reproducible from
the source: opening the overlay (`Ctrl+X Q` or clicking the
indicator) with two or more queued items immediately paints the
white rows. Wave-2 rubric: "Any RGB whose luminance > 160 is a
finding regardless of where it appears in the UI."

Fix: replace `Color::White` with `palette::muted_fg()` (or `QUIET`)
so the non-selected rows fade and the `GOLD`-bold selected row
remains the only stress point.

### 4. Portkey provider-config error during the portkey scenario

- **Provider:** portkey
- **Severity:** medium (per the wave-2 dispatch rule that provider-config
  errors are medium, not abort)
- **Dimension:** Cross-model consistency
- **File:** `crates/squeezy-eval/src/driver.rs` (provider resolution),
  operator-side `~/.squeezy/settings.toml`
- **Ticket:** `squeezy-vwlm`

`wave2-12-prompt-queue-portkey.toml` aborts before any keystroke:

```
squeezy-eval: provider: provider is not configured: missing PORTKEY_API_KEY
or SQUEEZY_PORTKEY_KEY; set the env var or add `[providers.<name>] api_key
= "…"` to ~/.squeezy/settings.toml or the project-local settings.toml
hint: for an offline run, set `[squeezy] provider = "mock"` in your scenario
and add a `[mock]` block with scripted `turns`. See docs/internal/EVAL_HARNESS.md.
```

No run directory was created. The error string itself is actionable
and free of jargon — `messaging` passes here. The cross-provider
defect this records is purely that the wave-2 portkey leg cannot be
diff-ed against OpenAI / Anthropic until the operator's Portkey key
is configured in `~/.squeezy/settings.toml` (the wave-2 hard rule
explicitly forbids the agent from reading that file). Recorded as
medium per the dispatch instructions; remediation is operator-side.

## Wave-1 harness limitations recapped

Both wave-1 findings #1 and #3 reproduce here:

- **Harness #1**: `TuiHarness::send_key` calls `pump_until_idle` on
  both sides. The OpenAI run shows each Enter completing its full
  turn before the next keystroke arrives (3 turns × ~1s each, status
  back to `"ready"` between each). The Enter handler's queue arm at
  `crates/squeezy-tui/src/lib.rs:1509-1515` (`turn_rx.is_some() =>
  push onto prompt_queue`, `status = "queued ({})"`) is therefore
  unreachable from any `send_keys`-based wave-2 scenario, regardless
  of provider streaming latency.
- **Harness #3**: harness-driven prompts emit zero turn-level trace
  events (`UserMessage` / `TurnStarted` / `AssistantDelta` /
  `TurnCompleted`), so `frames.jsonl` stays empty (0 frames in both
  the OpenAI and Anthropic runs), and the rule engine
  (`duplicate_tool_call`, `expect_input_tokens`, etc.) cannot trip.
  Cost is reported as `$0.0000` even though the OpenAI turns clearly
  burned tokens (each cycle took ~1s).

These are out-of-scope for wave-2 product remediation but worth
keeping the wave-1 tickets cited so future scenario authors don't
re-discover them. The scenario's `tui_status_contains "session"`
check at step 7 is also load-bearing on harness #3: it was authored
expecting a queue-state status string that the live event flow never
produces in this code path.

## Bundle

Bundles in each run directory under `tickets/` are absent (no rule
fired during the harness-driven run; the Anthropic 400s drop into
the action_step status field rather than a frame-level `Failed`
event the rule engine can key off). All evidence above is in
`trace.jsonl` and the rendered status text.
