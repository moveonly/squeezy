# wave2-05-slash-config-screen

- **Area:** `/config` (alias `/options`) overlay — dispatch, render, save flow.
- **Plan:** [`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`](../EVAL_COVERAGE_PLAN_WAVE2.md) row 05.
- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-05-slash-config-screen-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-05-slash-config-screen-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-05-slash-config-screen-portkey.toml`
- **Run dirs:**
  - `target/eval/wave2-05-slash-config-screen-openai-1780143965883`
  - `target/eval/wave2-05-slash-config-screen-anthropic-1780143981453`
  - `target/eval/wave2-05-slash-config-screen-portkey-1780144056419` (errored — provider config)
- **Source surfaces:**
  - `crates/squeezy-tui/src/config_screen/render.rs`
  - `crates/squeezy-tui/src/config_screen.rs` (helpers)
  - `crates/squeezy-tui/src/config_screen/save.rs`
  - `crates/squeezy-tui/src/lib.rs:882` `apply_external_settings_reload`
  - `crates/squeezy-tui/src/lib.rs:1824` `toggle_config_screen`

## Summary

Five defects surfaced across the three providers; four were ticketed via
`bd create`. A fifth (api-key env-var canonical-name mismatch) was
authored but blocked by the auto-mode classifier before submission and
is captured here for follow-up.

Cross-model behaviour: OpenAI completed all overlay assertions except
the `Local` tab; Anthropic mirrored the OpenAI defects **and** crashed
the post-overlay LLM exchange on a thinking-budget conflict; Portkey
errored out before any frame because the harness has no API key
configured (medium per task rule).

## Defect 1 — Local tab clipped at width=140 by Repo subtitle

- **Headline:** Local tab pushed off the tab strip by the Repo
  subtitle path + the hard-coded `(committed)` suffix.
- **Providers:** OpenAI, Anthropic (Portkey: scenario errored
  earlier).
- **Severity:** **P1** — actively hides a top-level navigation
  affordance.
- **Rubric dimension:** Visual clarity + Functionality.
- **`file:line`:** `crates/squeezy-tui/src/config_screen/render.rs:90-118`
  (`render_tabs` Span assembly; no truncation budget).
- **Beads:** `squeezy-5wu`.

Evidence — `target/eval/wave2-05-slash-config-screen-openai-1780143965883/trace.jsonl`
seq 12 `asserted_fail: frame does not contain "Local"`. The captured
preview:

```
Config   │ User ● ~/.squeezy/settings.toml ▸ Repo ○ ~/esqueezy/squeezy/.claude/worktrees/agent-a8f9491133db8e8fa/squeezy.toml (committed)
```

Notice the row ends after `(committed)` — no `▸ Local …` follows. The
preview also confirms the issue is independent of the active LLM
(same row produced verbatim on the Anthropic run, seq 12).

Suggested fix: truncate `display_path()` against the available
column budget before assembling the row, or drop subtitle paths on
inactive tabs once the budget is exhausted.

## Defect 2 — `Color::White` used pervasively in render.rs (palette guardrail)

- **Headline:** Inactive-state foreground is `Color::White` in 14
  spans across the `/config` render module, violating the wave-2
  palette luminance guardrail (luminance must be ≤ ~160).
- **Providers:** OpenAI, Anthropic (same code path; Portkey would
  inherit if the scenario ran).
- **Severity:** **P1** — guardrail breach is the canonical wave-2
  finding type.
- **Rubric dimension:** Visual clarity.
- **`file:line`:** `crates/squeezy-tui/src/config_screen/render.rs`
  lines 69, 287, 396, 401, 484, 567, 621, 713, 723, 883, 903, 940,
  945, 1032.
- **Beads:** `squeezy-23a`.

`Color::White` evaluates to RGB `(255, 255, 255)`, luminance
`0.299*255 + 0.587*255 + 0.114*255 = 255` — far above the ~160
ceiling documented in
[`EVAL_COVERAGE_PLAN_WAVE2.md`](../EVAL_COVERAGE_PLAN_WAVE2.md#palette-guardrails-read-these-before-evaluating-visual-clarity).
The expected substitute is `muted_fg()` /
`footer_fg()` (TrueColor-aware mid-tone derived from the active
palette tone), or `Color::White` should be guarded behind the
`Modifier::DIM` style.

Per-callsite intent (from a read of the surrounding code):

| Line | Span | Replacement candidate |
| --- | --- | --- |
| 69   | Inactive tab labels | `muted_fg()` |
| 287  | Reset-confirm key list value before/after | `muted_fg()` |
| 396  | Secret-entry env_var | `muted_fg()` |
| 401  | Secret-entry display dots | `muted_fg()` |
| 484  | Search overlay inactive row | `muted_fg()` |
| 567  | Model picker inactive row | `muted_fg()` |
| 621  | Sidebar inactive label | `muted_fg()` |
| 713  | Field pane inactive label | `muted_fg()` |
| 723  | Field pane inactive value | `muted_fg()` |
| 883  | Enum editor inactive option | `muted_fg()` |
| 903  | OptionalEnum editor inactive label | `muted_fg()` |
| 940  | Bool editor `false` state | `muted_fg()` |
| 945  | Bool editor `true` state | `muted_fg()` |
| 1032 | Footer base paragraph style | `footer_fg()` |

## Defect 3 — Anthropic Haiku 4.5 turn 400s on max_output_tokens < thinking budget minimum

- **Headline:** Anthropic provider request rejects `max_output_tokens
  = 512` when thinking is enabled — provider returns
  `thinking.enabled.budget_tokens: Input should be greater than or
  equal to 1024`.
- **Providers:** Anthropic (cross-model regression — OpenAI
  completed the same prompt at the same budget).
- **Severity:** **P1** — blocks the post-overlay live exchange the
  wave-2 plan requires; surfaced as a wall-of-jargon provider 4xx
  with no remediation hint.
- **Rubric dimension:** Cross-model consistency, Functionality,
  Messaging.
- **`file:line`:** likely `crates/squeezy-llm/src/anthropic.rs`
  (thinking-budget assembly does not clamp against
  `max_output_tokens`).
- **Beads:** `squeezy-lys`.

Evidence —
`target/eval/wave2-05-slash-config-screen-anthropic-1780143981453/trace.jsonl`
seq 28:

```
provider request failed: 400 Bad Request:
  {"type":"error","error":{"type":"invalid_request_error","message":
   "thinking.enabled.budget_tokens: Input should be greater than or equal to 1024"},
   "request_id":"req_011CbYkWMe9RNUYwbJZBR27n"}; retry or check provider/network status
```

The same `max_output_tokens = 512` runs cleanly on `gpt-5.4-mini`.
Two minimal fixes: either disable thinking when
`max_output_tokens < 1024`, or auto-raise the budget to at least the
upstream minimum and warn the operator.

## Defect 4 — Repo tab hardcodes "(committed)" even when squeezy.toml is absent

- **Headline:** Repo tab subtitle always reads
  `<path> (committed)` even when the `squeezy.toml` file is missing
  on disk.
- **Providers:** OpenAI, Anthropic.
- **Severity:** **P2** — messaging defect; misleads the operator
  about what is/isn't tracked in git.
- **Rubric dimension:** Messaging.
- **`file:line`:** `crates/squeezy-tui/src/config_screen/render.rs:103-110`.
- **Beads:** `squeezy-s5e`.

Evidence — preview captured at trace seq 12:

```
Repo ○ ~/esqueezy/squeezy/.claude/worktrees/agent-a8f9491133db8e8fa/squeezy.toml (committed)
```

The `○` glyph indicates *file absent* (see `render_tabs::tab`
lines 71-79), but the literal word "committed" reads as if a Repo
tier file is tracked in the working tree. Compounds with Defect 1:
the suffix is part of what pushes `Local` off the screen.

Suggested fix: only append `(committed)` when
`std::fs::metadata(&state.sources.project_path_default).is_ok()`,
or strip the suffix entirely and rely on the dot.

## Defect 5 (not filed — bd permission blocked) — api_key row shows "unset" when ANTHROPIC_API_KEY is exported

- **Headline:** Default Anthropic `api_key_env` is
  `SQUEEZY_ANTHROPIC_KEY`
  (`crates/squeezy-core/src/lib.rs:543`), but the canonical name
  users export is `ANTHROPIC_API_KEY`. The config-screen api-key row
  probes only the configured env var, so it reports `unset
  (SQUEEZY_ANTHROPIC_KEY) [unset · anthropic]` even when the live
  Anthropic provider client is authenticating via
  `ANTHROPIC_API_KEY`.
- **Providers:** Anthropic (and by symmetry would affect
  OpenAI/Google/etc. depending on default env-var names).
- **Severity:** **P2** — the screen lies about credential state.
- **Rubric dimension:** Messaging, Functionality (operator cannot
  trust the indicator).
- **`file:line`:** `crates/squeezy-tui/src/config_screen/render.rs:746-763`
  (api-key synthetic row branch).
- **Beads:** *not filed — `bd create` denied by auto-mode classifier
  after defect 4. Recommend filing manually.*

Evidence —
`target/eval/wave2-05-slash-config-screen-anthropic-1780143981453/trace.jsonl`
seq 12 preview row:

```
api_key  unset (SQUEEZY_ANTHROPIC_KEY) [unset · anthropic]
```

Yet the same run's later prompt reached the Anthropic API (the
400 surfaced was on the thinking budget, not auth), proving the
provider client had a working credential resolved from a different
env var.

## Provider configuration error (Portkey)

Per the task's "Provider config error → `medium` finding, not abort"
rule, the missing Portkey configuration is recorded here rather than
escalated. The Portkey scenario exited with:

```
provider: provider is not configured: missing PORTKEY_API_KEY or
SQUEEZY_PORTKEY_KEY; set the env var or add `[providers.portkey]
api_key = "…"` to ~/.squeezy/settings.toml or the project-local
settings.toml
```

Run dir
`target/eval/wave2-05-slash-config-screen-portkey-1780144056419/`
contains the empty `trace.jsonl` / `replay.tui` left behind.
Implication for the wave-2 plan: a third leg of cross-model
comparison was not collected; the OpenAI ↔ Anthropic diff above is
the available evidence.

## Repro

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-05-slash-config-screen-openai.toml \
  --no-triage

cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-05-slash-config-screen-anthropic.toml \
  --no-triage

cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-05-slash-config-screen-portkey.toml \
  --no-triage    # currently errors: missing PORTKEY_API_KEY
```

OpenAI summary: 29 trace events, 0 findings (rules), 0 frames, $0.0000
cost. The "Local" assertion failure registers as an `action_step`
status, not a rule-engine finding, because rendered-frame assertions
are scenario soft checks that record into the trace stream rather
than the `findings.jsonl` rule output.

Anthropic summary: 29 trace events, same scenario shape, same
visual-clarity defects, plus the thinking-budget crash on the
trailing prompt turn.

Portkey: provider config error (Beads: not filed; medium severity per
task rule). No frames.

## Filed Beads tickets

| Defect | Severity | Ticket |
| --- | --- | --- |
| 1. Local tab clipped at width=140 | P1 | `squeezy-5wu` |
| 2. Color::White used in 14 spans (palette guardrail) | P1 | `squeezy-23a` |
| 3. Anthropic 400 on max_output_tokens < thinking minimum | P1 | `squeezy-lys` |
| 4. Repo tab hardcodes "(committed)" | P2 | `squeezy-s5e` |
| 5. api_key row mislabels env state (Anthropic canonical) | P2 | *blocked — file manually* |
