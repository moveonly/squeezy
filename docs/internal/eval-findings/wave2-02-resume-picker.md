# wave2-02-resume-picker — eval findings

## Area

`02 resume-picker-and-restore` (per `docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`).

## Scenarios executed

| Provider | Scenario | Run dir | Outcome |
|---|---|---|---|
| openai | `crates/squeezy-eval/fixtures/scenarios/wave2-02-resume-picker-openai.toml` | `target/eval/wave2-02-resume-picker-openai-1780143954932/` | Live turn succeeded (`● ok` painted); 4 findings below. |
| anthropic | `crates/squeezy-eval/fixtures/scenarios/wave2-02-resume-picker-anthropic.toml` | `target/eval/wave2-02-resume-picker-anthropic-1780144145787/` | Provider 400 on first call — `thinking.enabled.budget_tokens` violation. |
| portkey | `crates/squeezy-eval/fixtures/scenarios/wave2-02-resume-picker-portkey.toml` | (no run dir — bailed at config-load) | Provider config error: `missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY`. |

The Portkey leg never reached the harness; it is recorded as a medium finding per the wave-2 triage rules. The Anthropic leg drained two `transcript entries` but the assistant call returned a 400 before producing tokens.

## Probe focus recap

The scenarios target the post-#151/#153 `Resuming session…` placeholder (`crates/squeezy-tui/src/lib.rs:11817`), the picker row legibility (`crates/squeezy-tui/src/resume_picker.rs`), and the first interactive frame after `Agent::resume` (`crates/squeezy-agent/src/lib.rs:1213`). Because `TuiHarness::new` (`crates/squeezy-tui/src/testing.rs:51`) only calls `Agent::new`, the picker overlay and the placeholder are not directly observable. Each scenario therefore drives a single live turn and asserts on the chrome of the *first interactive* frame; the `◆`/`▸` assertions guard the resume-picker brand identity at the source level (see Finding 5).

---

## Finding 1: anthropic: thinking budget_tokens clamp produces sub-1024 budget, 400 invalid_request_error

### Severity

**high** — every Anthropic reasoning-capable model fails immediately when the user's settings.toml has a `reasoning_effort` and the scenario or env caps `max_output_tokens` below 1024.

### What you should see vs. what you see

- Expected: turn streams the reply `ok`, ends with `Completed`.
- Observed: turn fails immediately with `provider request failed: 400 Bad Request: {"type":"error","error":{"type":"invalid_request_error","message":"thinking.enabled.budget_tokens: Input should be greater than or equal to 1024"}}`.

### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-02-resume-picker-anthropic.toml --no-triage
```

### Evidence

- `target/eval/wave2-02-resume-picker-anthropic-1780144145787/trace.jsonl` seq 1 status:
  `drained · 2 transcript entries · status="provider request failed: 400 Bad Request: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"thinking.enabled.budget_tokens: Input should be greater than or equal to 1024\"},\"request_id\":\"req_011CbYkiRvCwcj8JsYL9BTpT\"}"`.
- Trace seq 5 frame preview shows `⚠ turn failed: provider request failed: 400 Bad Request...` rendered in the TUI; the error reaches the user but cites Anthropic's raw payload, not a next step.

### Suspected cause

`crates/squeezy-llm/src/anthropic.rs:140`:

```rust
let budget = u64::from(effort.thinking_budget_tokens()).min(max_tokens.saturating_sub(1));
body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
```

When `max_tokens = 256` (the scenario sets `max_output_tokens = 256`), the clamp produces `budget = 255`, well below Anthropic's documented 1024-token floor for thinking. The request is then rejected before any model call. The fix needs to either raise `max_tokens` to keep `budget ≥ 1024`, skip the thinking block when the budget would fall below the floor, or surface a config-time validation error. The current user-facing message is the raw Anthropic JSON, which fails the **Messaging** rubric (no next step cited).

### Rubric dimensions

Functionality + Messaging.

### Ticket

`bd squeezy-0h7` (P1).

---

## Finding 2: tui: bright `Color::White` text violates the dark-only palette across startup card, user prompt, and resume picker

### Severity

**medium** — every first-interactive frame on every provider violates the wave-2 luminance ≤ 160 rule on at least four spans (`>_ Squeezy v0.1.0`, the `directory:` value, the `languages:` value, and the user prompt `> Reply with the single word ok.`). The picker overlay (when reachable) layers on two more.

### What you should see vs. what you see

- Expected: every cell in the first interactive frame uses one of the palette constants (`AMBER`, `GOLD`, `MODE_PURPLE`, `QUIET`, `SUCCESS_GREEN`, etc.) per `EVAL_COVERAGE_PLAN_WAVE2.md`; the rule of thumb requires luminance `0.299*R + 0.587*G + 0.114*B ≤ 160`.
- Observed: ~32 sites in `crates/squeezy-tui/src/lib.rs` plus `resume_picker.rs` paint with `Color::White` (255,255,255 → luminance 255), which the wave-2 brief calls out as a finding regardless of location.

### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-02-resume-picker-openai.toml --no-triage
```

Manual cross-check:

```sh
grep -n "fg(Color::White)" crates/squeezy-tui/src/lib.rs crates/squeezy-tui/src/resume_picker.rs
```

### Evidence

- `target/eval/wave2-02-resume-picker-openai-1780143954932/trace.jsonl` seq 5 frame preview:
  `│ >_ Squeezy v0.1.0 │ / │ model:  eval-harness:gpt-5.4-mini │ / │ directory: target/eval/_workspaces/snap--1780143954934 │ / │ languages: rust, python │ / > Reply with the single word ok. /  ● ok / ...`
- Offending source lines (file:line):
  - `crates/squeezy-tui/src/lib.rs:5518` — startup banner `>_ Squeezy v{version}` (`Color::White` + BOLD).
  - `crates/squeezy-tui/src/lib.rs:5531` — startup card `directory` value (`Color::White`).
  - `crates/squeezy-tui/src/lib.rs:5537` — startup card `languages` value (`Color::White`).
  - `crates/squeezy-tui/src/lib.rs:7078` — `Role::User` transcript body (`Color::White` on `PROMPT_BG`).
  - `crates/squeezy-tui/src/resume_picker.rs:546` — picker title segment "resume a recent session" (`Color::White`).
  - `crates/squeezy-tui/src/resume_picker.rs:639` — non-active picker row label style (`Color::White`).
- Same preview appears verbatim in `target/eval/wave2-02-resume-picker-anthropic-1780144145787/trace.jsonl` seq 5 — the violation is cross-provider and reproduces independently of the model.

### Suspected cause

`Color::White` is the most aggressive ANSI/TrueColor white the terminal can produce. The palette guardrail in `crates/squeezy-tui/src/render/palette.rs` defines dark text constants but doesn't ship a generic `TEXT_PRIMARY` token, so contributors reach for `Color::White` as the "default body text" colour. The fix is either a dim token (`Color::Rgb(220,220,220)` or `Color::Gray`) or `Color::Reset`, then sweep the ~32 sites.

### Rubric dimensions

Visual clarity (primary). Cross-model consistency (both OpenAI and Anthropic legs render the same offending spans).

### Ticket

`bd squeezy-3j5` (P2).

---

## Finding 3: eval: `drive_tui = true` scenarios write 0 frames, causing every `expect_final_text_contains` to false-positive

### Severity

**medium** — every wave-2 scenario that uses `[tui_capture] drive_tui = true` will report an `expect_final_text_contains` finding even when the model produced the required text.

### What you should see vs. what you see

- Expected: a scenario that pumps a turn through the TUI harness writes a `FrameRecord` per turn into `frames.jsonl` and a `TuiFrame` into `frames_tui.jsonl`, so the standard expectation rules ride the same artifact shape regardless of dispatch path.
- Observed: under `drive_tui = true`, both files are empty (0 lines / 0 bytes), `replay.tui` is empty, and the `expect_final_text_contains` rule fires unconditionally with `final assistant output missing required text: "ok"` even when the trace's frame preview clearly shows `● ok` painted.

### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-02-resume-picker-openai.toml --no-triage
```

Inspect:

```sh
wc -l target/eval/wave2-02-resume-picker-openai-1780143954932/frames.jsonl \
       target/eval/wave2-02-resume-picker-openai-1780143954932/frames_tui.jsonl
# both are 0
cat target/eval/wave2-02-resume-picker-openai-1780143954932/findings.jsonl
# {"rule_id":"expect_final_text_contains", "severity":"minor", ...}
```

### Evidence

- `target/eval/wave2-02-resume-picker-openai-1780143954932/findings.jsonl` line 1: `{"rule_id":"expect_final_text_contains","severity":"minor","summary":"final assistant output missing required text: \"ok\""}`.
- `target/eval/wave2-02-resume-picker-openai-1780143954932/trace.jsonl` seq 5 preview: `● ok` is visibly present in the harness's TUI.
- `crates/squeezy-eval/src/driver.rs:600` selects `run_prompt_through_harness` over `run_prompt` when `self.harness.is_some()`; only `run_prompt` writes frames (lines 1236 and 1837–1841).

### Suspected cause

`run_prompt_through_harness` in `crates/squeezy-eval/src/driver.rs:1052-1075` is fire-and-forget — it pumps the harness to idle and records a single `harness_prompt` ActionStep but never assembles a `FrameRecord`. The fix is to extract the final assistant text from the harness transcript after `pump_until_idle()` and emit both a `FrameRecord` (for `frames.jsonl` + downstream rules) and a `TuiFrame` (for `frames_tui.jsonl` + `replay.tui`).

### Rubric dimensions

Functionality (harness-side regression). Cross-model consistency: affects all three provider legs equally.

### Ticket

`bd squeezy-bnz` (P2).

---

## Finding 4: eval: `FrameCell` strips fg/bg so the dark-only palette luminance rule cannot be auto-asserted

### Severity

**low** — coverage gap; blocks the wave-2 visual-clarity rubric from being mechanised but doesn't itself produce wrong output.

### What you should see vs. what you see

- Expected: scenarios can write `[[steps.check]] kind = "tui_cell_luminance_le" max_luminance = 160` (or equivalent) and the harness can fail the assertion when any rendered cell crosses 160.
- Observed: `FrameCell` only stores `{ x, y, symbol }`. Style is intentionally omitted (`crates/squeezy-tui/src/testing.rs:251-253`).

### Reproducer

Read source. There is no live reproducer because the limitation is in the harness API.

### Evidence

- `crates/squeezy-tui/src/testing.rs:255-259`:
  ```rust
  pub struct FrameCell {
      pub x: u16,
      pub y: u16,
      pub symbol: String,
  }
  ```
- `crates/squeezy-eval/src/scenario.rs` has no `tui_cell_luminance_le` assertion variant.

### Suspected cause

Wave-1 shipped a `v1` surface that intentionally deferred style capture; wave-2 demands it.

### Rubric dimensions

Visual clarity (the rubric exists but cannot be enforced).

### Ticket

`bd squeezy-154l` (P3).

---

## Finding 5: portkey: provider config error blocks the third leg

### Severity

**medium** — cross-model consistency cannot be evaluated for this domain because only two of three legs ran. Per the wave-2 brief: "A provider config error becomes a medium finding, never an abort."

### What you should see vs. what you see

- Expected: with `[providers.portkey].api_key` configured in `~/.squeezy/settings.toml`, the scenario runs to completion against `@openrouter/qwen/qwen3.6-35b-a3b`.
- Observed: eval bails at config-load with `provider: provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add [providers.<name>] api_key = "…"`. No run directory is created.

### Reproducer

```sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-02-resume-picker-portkey.toml --no-triage
```

### Evidence

- The CLI stderr line (transcribed above) is the only artifact.
- The resolver path in `crates/squeezy-llm/src/credentials.rs:45-97` puts inline TOML keys ahead of env vars, so an `api_key` set under `[providers.portkey]` should resolve. The fact that it does not suggests either a worktree-cwd tier-ordering issue in `crates/squeezy-core/src/lib.rs:7392 load_settings_from_paths` when the snapshot workspace re-roots `find_project_settings_path`, or the user simply hasn't set the inline key.

### Suspected cause

Either the snapshot workspace cwd shifts the project settings search away from a tier that holds the key, or the user-tier (`~/.squeezy/settings.toml`) does not actually carry `[providers.portkey].api_key`. Per the task hard rules, the agent cannot read that file to verify; this finding records the failure and proposes the trace for whoever has the access.

### Rubric dimensions

Functionality, Cross-model consistency.

### Ticket

`bd squeezy-bhgy` (P3).

---

## Cross-scenario notes

### Brand identity assertions (`◆`, `▸`) cannot be observed on the first interactive frame

Both the OpenAI and Anthropic runs report `asserted_fail` on the `tui_frame_contains "◆"` and `tui_frame_contains "▸"` assertions. These glyphs live only inside the resume-picker overlay (`crates/squeezy-tui/src/resume_picker.rs:540, 641, 704`) and the reasoning chip (`crates/squeezy-tui/src/lib.rs:6613`). Neither path is reachable from a single live turn against `TuiHarness::new` — the harness has no `with_existing_session` constructor (`crates/squeezy-tui/src/testing.rs:51`). This is the documented wave-1 harness gap (`docs/internal/eval-findings/session-resume-picker.md` "Harness coverage gap"), not a production defect. The scenario stays as a regression guard so that once the harness lands an `Agent::resume` entry point, the picker chrome can be asserted directly.

### What the first interactive frame *did* paint

From `target/eval/wave2-02-resume-picker-openai-1780143954932/trace.jsonl` seq 5 preview (whitespace re-flowed):

```
│ >_ Squeezy v0.1.0                                            │
│ model:     eval-harness:gpt-5.4-mini                         │
│ directory: target/eval/_workspaces/snap--1780143954934       │
│ languages: rust, python                                      │
> Reply with the single word ok.
  ● ok
─ Worked for 1s ──────────────────────────────────────────────...
 ●  ┃
eval-harness:gpt-5.4-mini · target/eval/_workspaces/snap--1780143954934 · eb5ba60 · cost $0.004576    Build mode (Shift+Tab to cycle)
Enter send · !cmd shell · Up/Down menu/history · Ctrl+...
```

The frame is functionally healthy — startup card, prompt echo, assistant reply, working-card summary, status line, hint footer all paint. The findings above target the *visual* and *infrastructure* failures, not the agent loop itself.

---

## Manifest

- Scenarios: `crates/squeezy-eval/fixtures/scenarios/wave2-02-resume-picker-{openai,anthropic,portkey}.toml` (unchanged from the partial wave-2 run that landed in #156).
- Run dirs cited:
  - `target/eval/wave2-02-resume-picker-openai-1780143954932/`
  - `target/eval/wave2-02-resume-picker-anthropic-1780144145787/`
  - (portkey: no run dir — config error before scratch dir creation).
- Bd tickets: `squeezy-0h7`, `squeezy-3j5`, `squeezy-bnz`, `squeezy-154l`, `squeezy-bhgy`.
- Wave-1 antecedent: `docs/internal/eval-findings/session-resume-picker.md` (harness gap).
