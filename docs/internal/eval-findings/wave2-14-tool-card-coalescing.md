# wave2-14-tool-card-coalescing

Domain 14 of the wave-2 bug-hunt (`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`).
Authored from scratch — this domain produced no scenarios in the
partial wave. Three live scenarios were written + run and the
findings below cite per-provider trace evidence.

## Probe shape

Brief-mandated prompt across all three providers:

> Run grep for these five patterns one at a time and list which files
> matched each: `Agent`, `Turn`, `Skill`, `Plan`, `Tool`.

The surface under test is the render-time grouping path
`crates/squeezy-tui/src/lib.rs:6694` (`tool_run_info`) → `:6847`
(`format_grouped_tool_result_entry`). When ≥ 2 adjacent same-tool
same-status entries are pushed into the transcript, they collapse into
one card with header `"Searched N searches"` (plural noun from
`grouped_action_noun` at `:6921`) followed by per-member summary rows
and the affordance row `"(Ctrl-E to expand all)"` (`:6874`). Palette
guardrail: every cell in the grouped card stays `QUIET` (`DarkGray`)
except the lead action marker / verb which uses the lead tool's
display color.

## Run summary

| Scenario | Run directory | Trace events | Frames | Outcome |
|---|---|---:|---:|---|
| `wave2-14-tool-card-coalescing-openai` | `target/eval/wave2-14-tool-card-coalescing-openai-1780145639678` | 1 | 0 | `harness pump: agent error: pump_until_idle: did not reach idle within 180s` — turn parked on first `ApprovalRequested` (grep), TUI never re-entered idle, drive_tui pump deadline tripped |
| `wave2-14-tool-card-coalescing-anthropic` | `target/eval/wave2-14-tool-card-coalescing-anthropic-1780145863697` | 1 | 0 | same as openai — `pump_until_idle: did not reach idle within 180s` |
| `wave2-14-tool-card-coalescing-portkey` | `target/eval/wave2-14-tool-card-coalescing-portkey-1780146053112` | 0 | 0 | provider config error before any step: `missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY` (key is in `~/.squeezy/settings.toml` per brief; resolver does not honour it) |

A supplemental non-drive_tui probe (`/tmp/wave2-14-openai-no-tui.toml`
→ `target/eval/wave2-14-openai-no-tui-probe-1780146104005`) was run
against gpt-5.4-mini to confirm the agent loop and the approval wall
without the harness's 180s ceiling. The model emitted 2 tool calls
(1× `definition_search`, 1× `grep`); both were auto-denied by
`Driver::decide_approval` because the scenario carried no Approve
actions. The agent then capitulated with prose ("I can't run the
workspace grep here…") and the turn finished after 60s wall clock.
This confirms that even outside drive_tui, the wave-2 brief's
`permission_mode = "allow"` overlay alone is insufficient — the
overlay only flips `edit`, `shell`, `web`, `mcp`
(`crates/squeezy-eval/src/driver.rs:476-485`), not `read` /
`search`, so search-class tools still hit the approval path.

## Defects

### 01 — TuiHarness `pump_until_idle` deadlocks at 180s when a tool requires approval (`drive_tui = true`)

#### Severity

medium — this is the gating defect for wave-2 domain 14. The probe
asks the agent to issue 5 sequential `grep` calls so the rendered
transcript can be inspected for the grouped coalesced card. With
`drive_tui = true`, the agent's first `ApprovalRequested` is stashed
in `app.pending_approval` and the harness pump never reaches an idle
predicate. After 180s the pump returns `Err`, the scenario tears down,
and `frames_tui.jsonl` / `replay.tui` are empty. No coalescing
observation is possible against any live provider until this lands.

Cross-provider: reproduces against `openai gpt-5.4-mini` and
`anthropic claude-haiku-4-5-20251001` with identical wave-2 probe
inputs. Mock-provider reproducer with five pre-armed `Approve`
actions also times out, confirming the harness path does not consume
the eval driver's action_queue at all.

#### What you should see vs. what you see

- Expected: the harness either (a) auto-approves any
  `ApprovalRequested` by walking the eval driver's action_queue
  the same way `Driver::decide_approval` does on the non-drive_tui
  path, OR (b) surfaces the pending approval as a pump return state
  the scenario can react to via `send_key`. With either path the
  agent can complete its 5 `grep` calls and the rendered transcript
  shows the grouped card `"Searched 5 searches"` + `"(Ctrl-E to
  expand all)"`.
- Observed: pump_until_idle blocks for the full 180s, errors with
  `pump_until_idle: did not reach idle within 180s`, and the
  scenario aborts. No subsequent `assert` steps execute.

#### Rubric dimension

Functionality (harness instrumentation) + Cross-model consistency
(the same blocker on both openai and anthropic).

#### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-14-tool-card-coalescing-openai.toml \
  --no-triage
```

Same shape against the anthropic and portkey scenarios.

#### Evidence

- `target/eval/wave2-14-tool-card-coalescing-openai-1780145639678/trace.jsonl`
  contains exactly one record (`{"kind":"action_step",
  "action":{"index":1,"kind":"step_boundary","step_kind":"prompt"},
  "status":"started"}`) before the 180s deadline.
- Same shape in
  `target/eval/wave2-14-tool-card-coalescing-anthropic-1780145863697/trace.jsonl`.
- Mock-provider reproducer
  (`/tmp/wave2-14-mock-probe2.toml`, 5 pre-armed `[steps.match]
  tool = "grep"` Approves) also blocks; confirms the eval driver's
  action_queue is not connected to the TUI's
  `app.pending_approval` consumer in drive_tui mode.
- Source: `crates/squeezy-tui/src/testing.rs:102` hard-codes the
  180s deadline; `crates/squeezy-tui/src/lib.rs:1805`
  is the production approval consumer (`take()` from
  `app.pending_approval`). `Driver::decide_approval` at
  `crates/squeezy-eval/src/driver.rs:2030-2059` is the eval-side
  decision-maker that drive_tui never invokes.
- Beads: `squeezy-tje9`.

#### Suspected cause

`crates/squeezy-tui/src/testing.rs:96-136` (`pump_until_idle`) drains
job/agent/diff events but has no path to consume a TUI approval modal.
Production handles this via a user keystroke through `handle_key`.
Eval scenarios drive approvals through `Driver::decide_approval`
which only runs on the non-drive_tui dispatch (`run_prompt`, not
`run_prompt_through_harness`). The fix wants the harness to either:

1. Drain `app.pending_approval` in each pump tick and invoke a
   caller-provided approval callback (eval would route to its
   action_queue), or
2. Expose `pending_approval` as a return state so a wrapping
   scenario can issue the canonical Enter keystroke.

Until then, every wave-2 scenario that needs to observe tool
behaviour through the live-TUI render — coalescing, retry badging,
working-card mid-stream — is wedged on the approval wall.

---

### 02 — Portkey provider config rejects `[providers.portkey].api_key` in `~/.squeezy/settings.toml`

#### Severity

medium (per wave-2 hard rules: "Provider config error → `medium`
finding, not abort"). Cross-provider regression: openai + anthropic
resolved without touching env; portkey did not.

#### What you should see vs. what you see

- Expected: `AppConfig::from_env_and_settings_with_provider("portkey")`
  reads `[providers.portkey].api_key` out of `~/.squeezy/settings.toml`
  the same way the production CLI does, and the scenario proceeds to
  drive `@openrouter/qwen/qwen3.6-35b-a3b`.
- Observed: scenario aborts before any step runs with `provider:
  provider is not configured: missing PORTKEY_API_KEY or
  SQUEEZY_PORTKEY_KEY; set the env var or add [providers.<name>]
  api_key = "…"`.

#### Rubric dimension

Functionality (provider plumbing) + Cross-model consistency.

#### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-14-tool-card-coalescing-portkey.toml \
  --no-triage
```

#### Evidence

- Stdout from the run (verbatim):
  ```
  ▶ squeezy-eval running: Wave-2 / domain 14 — tool card coalescing on Portkey/OpenRouter Qwen3 (wave2-14-tool-card-coalescing-portkey)
  squeezy-eval: provider: provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add `[providers.<name>] api_key = "…"` to ~/.squeezy/settings.toml or the project-local settings.toml
  ```
- `target/eval/wave2-14-tool-card-coalescing-portkey-1780146053112/trace.jsonl`
  is 0 bytes (no events captured); no `run.json` emitted.
- Per the wave-2 hard rules, the agent did **not** inspect
  `~/.squeezy/settings.toml`. The dispatch brief states the key is
  there.
- Suspected source: `AppConfig::from_env_and_settings_with_provider`
  (called from `crates/squeezy-eval/src/driver.rs`) skips the
  `[providers.portkey].api_key` block when validating
  `ProviderConfig.api_key_env`. Same root cause cluster as
  `squeezy-5ce`, `squeezy-hg94`, `squeezy-67j`, `squeezy-j8yi`.
- Beads: `squeezy-petb`.

#### Suspected cause

Same shape as wave2-01 / wave2-03 / wave2-15 portkey findings. Until
the resolver reads `[providers.<name>].api_key` for non-env-key
providers, no wave-2 portkey scenario can run.

---

## Defects considered and dismissed

- **`permission_mode = "allow"` doesn't extend to `read` / `search`
  capabilities.** Reproducible — `Driver::apply_overlay`
  (`crates/squeezy-eval/src/driver.rs:476-485`) only flips
  `edit`/`shell`/`web`/`mcp`. The supplemental non-drive_tui probe
  showed `grep` + `definition_search` auto-denied even with
  `permission_mode = "allow"` and the resulting
  `[approval_unanswered]` + `[denied_tool_call_ux]` findings (4 in
  total) in `target/eval/wave2-14-openai-no-tui-probe-1780146104005/run.json`.
  Already covered by `squeezy-bsr0` (same defect for
  `read_tool_output`). Not re-filed; flagged here so the next operator
  doesn't re-discover it.

- **The brief's "(5x)" indicator is rendered as "Searched 5
  searches" + `(Ctrl-E to expand all)`, not `(5x)`.** The bare `(Nx)`
  badge is from the *push-time* retry coalescer
  (`coalesce_tool_transcript_entry`, `crates/squeezy-tui/src/lib.rs:11525`)
  which fires only when the same tool retries the same path with the
  same status (e.g. apply_patch repeating the same "search text not
  found" error). Five distinct grep patterns hit the *render-time*
  group path (`tool_run_info` → `format_grouped_tool_result_entry`)
  whose header noun comes from `grouped_action_noun`
  (`crates/squeezy-tui/src/lib.rs:6921`). Both surfaces are
  considered "the coalescing indicator" — they just live in different
  code paths. Not a defect; documented so reviewers don't expect a
  literal `(5x)` string in the rendered frame.

- **`frames.jsonl` empty across all three runs.** Known consequence
  of `drive_tui = true` routing prompts through
  `TuiHarness::start_user_turn` / `pump_until_idle` instead of the
  normal frame-emitting path. Documented in wave2-01 finding; not
  re-filed.

## Operator notes

- The wave2-14 OpenAI + Anthropic runs each consumed ~180s of wall
  clock waiting on the pump deadline; cost stayed effectively at $0
  because no LLM call completed past the first approval request.
- All three runs use `[tui_capture] enabled = true, drive_tui =
  true, palette_tone = "dark"`. Even though the assertions never
  execute, the captured trace event is enough to prove the harness
  blocked on `step_boundary started` without progressing.
- The non-drive_tui supplemental probe (cost $0.0049, 108 trace
  events) is the practical floor for what's observable today: it
  records the model's intent (calls were attempted) but not the
  rendered coalesced card.
- Severity assignments above all follow the wave-2 triage rule:
  cross-provider regressions and findings backed by a `trace.jsonl`
  seq earn a `medium` minimum. Nothing here is `low` or `[flaky]`.
