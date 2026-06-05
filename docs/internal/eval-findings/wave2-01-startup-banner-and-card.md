# wave2-01-startup-banner-and-card

Domain 01 of the wave-2 bug-hunt (`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`).
Re-dispatched after the rate-limit failure documented in
`docs/internal/eval-findings/wave2-runbook.md`.

## Run summary

| Scenario | Run directory | Trace events | Frames | Findings (auto) | Outcome |
|---|---|---:|---:|---:|---|
| `wave2-01-startup-banner-and-card-openai` | `target/eval/wave2-01-startup-banner-and-card-openai-1780143932257` | 10 | 0 | 0 | `asserted_fail` on model row (provider name = `eval-harness`, not `openai`); other asserts pass |
| `wave2-01-startup-banner-and-card-anthropic` | `target/eval/wave2-01-startup-banner-and-card-anthropic-1780144011076` | 10 | 0 | 0 | provider returned `400 Bad Request: thinking.enabled.budget_tokens: Input should be greater than or equal to 1024`; turn failed, then same model-row assertion failed |
| `wave2-01-startup-banner-and-card-portkey` | `target/eval/wave2-01-startup-banner-and-card-portkey-1780144063978` | 0 | 0 | 0 | provider config errored before any step: `missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY` despite `[providers.portkey].api_key` in `~/.squeezy/settings.toml` per the dispatch brief |

All three runs used `[tui_capture] enabled = true, drive_tui = true,
width = 120, height = 36, palette_tone = "dark"`. `frames.jsonl` is
empty across the board because the `drive_tui` path bypasses the
per-turn frame writer; everything below is read from `trace.jsonl`'s
`tui_frame_contains` preview, which is the literal rendered frame.

## Defects

### 01 — Startup card values rendered in `Color::White` (luminance 255) violate the dark-amber palette guardrail

#### Severity

medium — every wave-2 scenario that drives the TUI on a dark terminal trips this; the value rows (`>_ Squeezy v…`, `directory:`, `languages:`) are the first thing a new user sees and they read as bright-on-black, the exact thing `EVAL_COVERAGE_PLAN_WAVE2.md` palette guardrails prohibit.

#### What you should see vs. what you see

- Expected: every cell in the startup card has luminance `≤ 160` per `0.299R + 0.587G + 0.114B`. Brand-significant cues (banner title) use `AMBER` (luminance 156); chrome and neutral values use `QUIET`, `muted_fg()`, or `GOLD`.
- Observed: the banner title `>_ Squeezy v0.1.0` and the `directory` / `languages` value spans are bound to `Color::White` (RGB 255, 255, 255 → luminance 255), well above the 160 cap.

#### Rubric dimension

Visual clarity.

#### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-01-startup-banner-and-card-openai.toml \
  --no-triage
```

Look at `trace.jsonl` seq=5 preview field.

#### Evidence

- `target/eval/wave2-01-startup-banner-and-card-openai-1780143932257/trace.jsonl` seq=5 preview field:
  ```
  │ >_ Squeezy v0.1.0                                            │ /
  │ model:     eval-harness:gpt-5.4-mini                         │ /
  │ directory: target/eval/_workspaces/snap--1780143932258       │ /
  │ languages: rust, python                                      │
  ```
  (the corresponding `startup_card_lines` spans bind `Color::White` to the bold title and to the directory / languages values.)
- Source: `crates/squeezy-tui/src/lib.rs:5517` (banner title), `:5531` (directory value), `:5537` (languages value).
- Beads: `squeezy-syp`.

#### Suspected cause

`crates/squeezy-tui/src/lib.rs:5504-5572` builds the startup card with
`Style::default().fg(Color::White)` for three of the four value spans.
Replacing those bindings with `palette::muted_fg()` (or — for the
brand title — `palette::AMBER`) keeps the rows readable on dark
terminals without breaking the 160-luminance ceiling.

---

### 02 — Anthropic Haiku 4.5: `thinking.enabled.budget_tokens < 1024` 400 when `max_output_tokens` is small

#### Severity

medium — any scenario or user setting `max_output_tokens ≤ ~1024` on Anthropic Haiku 4.5 produces a hard turn failure on the first request, with no graceful fallback or warning. Cross-model regression: openai and (when configured) portkey accept the same `max_output_tokens = 256` without complaint.

#### What you should see vs. what you see

- Expected: the agent either (a) raises `max_tokens` so the thinking budget can fit under the Anthropic minimum, (b) skips the `thinking` block entirely when `max_tokens ≤ 1024`, or (c) surfaces a configuration error pointing at `max_output_tokens` *before* the request hits the wire.
- Observed: the request is sent with `body.thinking.budget_tokens = 255` and Anthropic rejects with HTTP 400, surfacing as `turn failed: provider request failed: 400 Bad Request: thinking.enabled.budget_tokens: Input should be greater than or equal to 1024`.

#### Rubric dimension

Functionality + Cross-model consistency (anthropic regresses vs. openai).

#### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-01-startup-banner-and-card-anthropic.toml \
  --no-triage
```

#### Evidence

- `target/eval/wave2-01-startup-banner-and-card-anthropic-1780144011076/trace.jsonl` seq=1 status:
  ```
  drained · 2 transcript entries · status="provider request failed: 400 Bad Request:
  {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",
   \"message\":\"thinking.enabled.budget_tokens: Input should be greater than or equal to 1024\"},
   \"request_id\":\"req_011CbYkYYqQ1CxqsMnrh31HD\"}; retry or check provider/network status"
  ```
- Same run, trace seq=5 preview shows the resulting TUI frame includes `⚠ turn failed: provider request failed: 400 Bad Request:` — the message is at least user-visible, but the agent never recovers without a manual `max_output_tokens` raise.
- Source: `crates/squeezy-llm/src/anthropic.rs:140` binds `budget = u64::from(effort.thinking_budget_tokens()).min(max_tokens.saturating_sub(1))`. With `max_output_tokens = 256` and default `reasoning_effort = Low (4096)` for a thinking-capable model, that evaluates to `min(4096, 255) = 255`, below Anthropic's hard minimum of 1024.
- Model registry: `crates/squeezy-llm/src/models.json:154-166` marks `claude-haiku-4-5-20251001` with `capabilities.reasoning_effort: true`, so `request_reasoning_effort` (`crates/squeezy-agent/src/lib.rs:4123-4131`) forwards the effort and the thinking block is unconditionally added.
- Beads: `squeezy-irg`.

#### Suspected cause

`crates/squeezy-llm/src/anthropic.rs:140`. The clamp protects against
`budget >= max_tokens` (a different Anthropic constraint) but ignores
Anthropic's lower bound of 1024. The fix wants to either (a) drop the
`thinking` block when `max_tokens <= 1024`, or (b) clamp
`budget = max(1024, …)` and simultaneously raise `max_tokens` so the
upper-bound check still holds. Today the user-visible outcome is a
permanently failing turn.

---

### 03 — `TuiHarness` hard-codes provider_name "eval-harness", blocking provider-identity TUI assertions

#### Severity

medium — every eval scenario that uses `drive_tui = true` and asserts on the banner provider row will always fail, because the harness never threads the real provider name through to `TuiApp`. This makes the entire "cross-provider banner consistency" rubric dimension unobservable from eval.

#### What you should see vs. what you see

- Expected: when a scenario configures `[squeezy] provider = "openai"`, the rendered startup card's model row should read `openai:gpt-5.4-mini`, matching what `agent.provider_name()` returns in production (`crates/squeezy-tui/src/lib.rs:526`).
- Observed: the model row reads `eval-harness:<model>` regardless of provider. Same defect surfaces in the status line at the bottom of the TUI.

#### Rubric dimension

Cross-model consistency (provider name is meant to be the most basic cross-provider signal) and harness-instrumentation honesty.

#### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-01-startup-banner-and-card-openai.toml \
  --no-triage
```

#### Evidence

- `target/eval/wave2-01-startup-banner-and-card-openai-1780143932257/trace.jsonl` seq=5 status:
  ```
  asserted_fail: frame does not contain "openai:gpt-5.4-mini" ·
  preview: │ >_ Squeezy v0.1.0 … │ model:     eval-harness:gpt-5.4-mini … │
  ```
- Same defect in the anthropic run (`…-anthropic-1780144011076/trace.jsonl` seq=5: `eval-harness:claude-haiku-4-5-20251001`).
- Source: `crates/squeezy-tui/src/testing.rs:53` literally passes `"eval-harness"` as `provider_name` to `TuiApp::new_with_clipboard`.
- Production code passes `agent.provider_name()` (a `&'static str` from `squeezy_llm::provider_name`) at `crates/squeezy-tui/src/lib.rs:526`.
- Beads: `squeezy-p7f`.

#### Suspected cause

`crates/squeezy-tui/src/testing.rs:53`. `TuiHarness::new` should call
`squeezy_llm::provider_name(&config.provider)` and pass that
`&'static str` instead of a literal — same shape as production. Until
this lands, every wave-2 banner / status-line assertion that names the
provider has to be rewritten against the bogus `eval-harness` string,
defeating the test's purpose.

---

### 04 — Portkey eval scenario errors with `missing PORTKEY_API_KEY` despite key in `~/.squeezy/settings.toml`

#### Severity

medium (per dispatch hard rules: "If a provider returns missing FOO_API_KEY or any config error, treat it as a `medium` finding and move on"). Cross-provider regression: openai + anthropic resolved without touching env, portkey did not.

#### What you should see vs. what you see

- Expected: `AppConfig::from_env_and_settings_with_provider("portkey")` reads `[providers.portkey].api_key` out of `~/.squeezy/settings.toml` and proceeds to send a request to the `@openrouter/qwen/qwen3.6-35b-a3b` model.
- Observed: scenario aborts before any step runs: `squeezy-eval: provider: provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add [providers.<name>] api_key = "…" to ~/.squeezy/settings.toml or the project-local settings.toml`.

#### Rubric dimension

Functionality (provider plumbing) + Cross-model consistency.

#### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-01-startup-banner-and-card-portkey.toml \
  --no-triage
```

#### Evidence

- Stdout from the live run (no `run.json` was written — the run aborts during AppConfig resolution):
  ```
  ▶ squeezy-eval running: Wave-2 / domain 01 — startup banner + card on Portkey/OpenRouter Qwen3 (wave2-01-startup-banner-and-card-portkey)
  squeezy-eval: provider: provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add `[providers.<name>] api_key = "…"` to ~/.squeezy/settings.toml or the project-local settings.toml
  hint: for an offline run, set `[squeezy] provider = "mock"` in your scenario and add a `[mock]` block with scripted `turns`. See docs/internal/EVAL_HARNESS.md.
  ```
- `target/eval/wave2-01-startup-banner-and-card-portkey-1780144063978/trace.jsonl` is empty (0 events); no `run.json` was emitted.
- Per the dispatch hard rules, the agent **did not** inspect `~/.squeezy/settings.toml` to confirm the key shape. The brief states the key is there; the harness apparently doesn't consume the `[providers.portkey].api_key` path the same way the production resolver does.
- Source (suspected): `crates/squeezy-eval/src/driver.rs:130`. `AppConfig::from_env_and_settings_with_provider("portkey")` is being called but the portkey provider's `api_key_env` check still requires an env var when the settings file already has the key.
- Beads: `squeezy-67j`.

#### Suspected cause

`AppConfig::from_env_and_settings_with_provider` may not be honouring
the `[providers.portkey].api_key` block when validating the
`ProviderConfig.api_key_env` requirement. Production (the squeezy CLI)
appears to resolve the key fine from the same file. Without
remediation, no portkey-based wave-2 scenario can run.

---

### 05 — Eval `asserted_fail` action_steps never roll up into `findings.jsonl` / `run.json.totals.findings`

#### Severity

medium — CI gating via `squeezy-eval check --fail-on findings` silently passes scenarios whose `assert` actions failed. Affects every wave-2 scenario that uses scripted assertions, which is most of them.

#### What you should see vs. what you see

- Expected: an `action_step` whose `status` starts with `asserted_fail` produces a finding (rule `asserted_fail` or equivalent) so `run.json.totals.findings ≥ 1` and `findings.jsonl` carries one entry. CI can then fail the scenario.
- Observed: `target/eval/wave2-01-startup-banner-and-card-openai-1780143932257/run.json` reports `"findings": 0` even though `trace.jsonl` seq=5 status carries `asserted_fail: frame does not contain "openai:gpt-5.4-mini" …`. `findings.jsonl` is empty.

#### Rubric dimension

Harness instrumentation (the eval's own functional contract: `assert` should be observable in the totals).

#### Reproducer

Run any wave-2 scenario that has a failing `assert` step (e.g. the
openai run above). Inspect `run.json` — `totals.findings` will be 0
regardless of how many `asserted_fail` events the trace carries.

#### Evidence

- `target/eval/wave2-01-startup-banner-and-card-openai-1780143932257/run.json` showing `"findings": 0` despite an `asserted_fail` in the trace.
- Same defect in the anthropic run (`target/eval/wave2-01-startup-banner-and-card-anthropic-1780144011076/run.json` → `"findings": 0`).
- Source: `crates/squeezy-eval/src/findings.rs` registers rules for `unsupported_slash_command`, `approval_unanswered`, `repeated_turn_failure`, etc., but no rule iterates `ctx.action_steps` looking for `asserted_fail` prefixes the way `UnsupportedSlashCommand` (`crates/squeezy-eval/src/findings.rs:382`) does for its own status prefix.
- Beads: `squeezy-16d`.

#### Suspected cause

Adding a rule analogous to `UnsupportedSlashCommand` that fires when
`action_step.status.starts_with("asserted_fail")` would close the gap.
Severity probably defaults to `minor`; scenarios that want to gate CI
on a failed assertion already opt in via `--fail-on findings`.

---

## Defects considered and dismissed

- `frames.jsonl` is empty in all three runs. This is a known
  consequence of `drive_tui = true` routing user prompts through
  `TuiHarness::start_user_turn` / `pump_until_idle` instead of the
  normal frame-emitting path in `EvalDriver`. It is an
  instrumentation incompleteness, but `trace.jsonl`'s
  `tui_frame_contains` preview still carries the literal rendered
  frame, so the assertions remain checkable. Not filed; flagged here
  so the next operator knows not to re-discover it.
- The `languages: rust, python` value comes from
  `configured_language_summary` (`crates/squeezy-tui/src/lib.rs:10257`)
  which reads `config.graph.languages` straight from settings. That
  list is correct for this squeezy worktree; not a defect.
- The `>_ Squeezy v0.1.0` banner shows the literal package version,
  which matches `CARGO_PKG_VERSION`. Not a defect.

## Operator notes

- Each `cargo run -p squeezy-eval -- run …` invocation reuses the
  cold-build binary; the three runs above completed in 2-7 s wall
  clock once compilation finished.
- The portkey scenario short-circuits in <1 s with no run-dir
  artifacts beyond the empty `trace.jsonl`; treat that as the
  signature of finding 04 above.
- Severity assignments above all follow the wave-2 triage rule:
  cross-provider regressions and findings backed by a `trace.jsonl`
  seq earn a `medium` minimum; nothing here is `low` or `[flaky]`.
