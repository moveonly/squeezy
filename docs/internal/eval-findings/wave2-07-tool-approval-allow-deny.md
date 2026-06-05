# wave2-07 tool-approval allow / deny — eval finding report

- **Domain:** 07 tool-approval allow/deny (wave-2)
- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-07-tool-approval-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-07-tool-approval-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-07-tool-approval-portkey.toml`
- **Run dirs:**
  - `target/eval/wave2-07-tool-approval-openai-1780144019859/`
  - `target/eval/wave2-07-tool-approval-anthropic-1780144065917/`
  - `target/eval/wave2-07-tool-approval-portkey-1780144108280/` (empty — provider config error)
- **Date:** 2026-05-30

## Probe shape

Each scenario sets `permission_mode = "ask"` and queues three actions
before any prompt fires:

1. `approve` for `apply_patch`
2. `approve` for `write_file`
3. `deny` for `shell` with reason `"wave-2-07: deny shell …"`

Three prompts then walk the model through `edit → shell → ack`, so the
agent emits one `ApprovalRequested` for the edit (consumed by the
queued allow), one for shell (consumed by the queued deny, no
`tool_call_started` because the deny short-circuits dispatch), and a
final terse acknowledgement.

`format_approval_menu_lines` (`crates/squeezy-tui/src/lib.rs:4450`) and
`approval::render_preview`
(`crates/squeezy-tui/src/approval.rs:26`) compose the rendered card.

## Run-level summary

| Provider | Run dir | trace events | frames | findings | cost |
|---|---|--:|--:|--:|--:|
| openai gpt-5.4-mini | `wave2-07-tool-approval-openai-1780144019859` | 204 | 3 | 0 | $0.0105 |
| anthropic claude-haiku-4-5 | `wave2-07-tool-approval-anthropic-1780144065917` | 59 | 3 | 2 | $0.0212 |
| portkey @openrouter/qwen/qwen3.6-35b-a3b | (provider config error) | 0 | 0 | — | $0.00 |

Approval flow held on both live providers:

- `seq=92 (openai) / 17 (anthropic) kind=approval tool=apply_patch
  decision="approved"`; `tool_call_started` fires for `apply_patch`,
  one-line patch lands cleanly.
- `seq=186 (openai) / 37 (anthropic) kind=approval tool=shell
  decision="denied:wave-2-07: deny shell to verify the deny
  short-circuit"`; **no** `tool_call_started` for `shell`; the
  follow-up `tool_call_completed` carries `status="Denied"` +
  `permission_denied: true` and the model's response acknowledges the
  denial (no retry on the same call).

The Anthropic findings (`ungrounded_citation × 2`) fire because the
denied shell tool call leaves `frame.tool_calls = []` for turn 2/3
even though the model legitimately cites the file it created in
turn 1. That is a rule-side false-positive in
`crates/squeezy-eval/src/findings.rs` (the rule should treat denied
calls as evidence of action), not an approval-deny regression, so it
is not filed against this domain.

## Defects filed

### bd squeezy-9ui — approval menu rows use `Color::White` (luminance 255) [P1, Visual clarity]

- **File:line:** `crates/squeezy-tui/src/lib.rs:4460-4464`
- **Provider scope:** all three (constant-side; renders identically per provider)
- **Evidence:** `format_approval_menu_lines` styles non-selected option
  labels with `Color::White` (`Rgb(255, 255, 255)` → luminance 255).
  Per `docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md` palette guardrails any
  cell luminance > 160 is a finding. The option labels (`Approve`,
  `Approve for this session`, `Allow Project: …`, `Deny`, `Deny for
  this session`) are the most prominent text in the approval card;
  rendering them in pure white competes with the AMBER brand cue and
  the GOLD selected-row highlight in the same frame.
- **Suggested fix:** route non-selected rows through `muted_fg()`
  (already used elsewhere for tone-aware secondary text) or a similarly
  dark palette constant. The selected-row branch already uses GOLD
  without bold, which matches the rubric.

### bd squeezy-6fi — `DIFF_DEL_FG Rgb(252,165,165)` bold violates dark-only palette in approval diff preview [P1, Visual clarity + Diff readability]

- **File:line:** `crates/squeezy-tui/src/render/palette.rs:40`,
  `crates/squeezy-tui/src/render/diff.rs:241-247`
- **Provider scope:** all three (constant-side)
- **Evidence:** `DIFF_DEL_FG = Rgb(252, 165, 165)` → luminance
  `0.299*252 + 0.587*165 + 0.114*165 = 191.0`, well above the ~160
  threshold. `delete_fg_style()` applies it with `Modifier::BOLD` in
  the same render path used by `approval::render_preview`'s
  `unified_diff` branch (`crates/squeezy-tui/src/approval.rs:118-135`).
  On a multi-line patch the bright-coral bold deletion lines outshine
  the AMBER brand and the GOLD selected row in the same approval card.
- **Suggested fix:** rebalance `DIFF_DEL_FG` to luminance ≤ 160 (e.g.
  `Rgb(180, 80, 80)` — the existing `ERROR_RED` is exactly that). The
  soft `diff_del_bg()` tint already carries the deletion semantic; the
  fg can drop to a calmer hue.

### bd squeezy-9k9 — approval preview body lines (`plain_white`) use `Color::White` [P2, Visual clarity]

- **File:line:** `crates/squeezy-tui/src/approval.rs:215-220` (helper);
  call sites at `:113`, `:88`, `:150`, `:185`, `:192`, `:207`
- **Provider scope:** all three (constant-side)
- **Evidence:** the `plain_white` helper styles the preview body —
  edited file paths (`✎ <path>`), shell command (`$ <cmd>`),
  read-target path, MCP descriptor, rule-preview rule string — with
  `Color::White` (luminance 255). Every approval frame across all
  three providers renders 2–6 such lines bright white directly above
  the option menu. Cumulative effect: the approval card is dominated
  by pure-white text on a dark terminal, drowning the AMBER `Allow
  Project: …` label that is the only intentional brand cue in the
  block.
- **Suggested fix:** drop `plain_white` to `muted_fg()` (tone-aware,
  already used for secondary text) or define a new palette constant
  for primary preview text at luminance ≤ 160. The existing `dim()`
  helper handles secondary metadata and is fine as-is.

### bd squeezy-7bf — wave2-07 portkey scenario aborts: missing `PORTKEY_API_KEY` [P2, Cross-model consistency]

- **File:line:** `crates/squeezy-eval/fixtures/scenarios/wave2-07-tool-approval-portkey.toml`;
  config resolution in `squeezy-llm`/`squeezy-config`
- **Provider scope:** portkey only
- **Evidence:** `squeezy-eval` aborts immediately with
  `provider: provider is not configured: missing PORTKEY_API_KEY or
  SQUEEZY_PORTKEY_KEY; set the env var or add [providers.<name>]
  api_key = "…" to ~/.squeezy/settings.toml`. The Portkey run dir
  (`target/eval/wave2-07-tool-approval-portkey-1780144108280/`) holds
  only an empty `trace.jsonl` + `frames.jsonl`; no `run.json`. The
  cross-model-consistency rubric dimension cannot be evaluated for
  wave2-07. The wave-2-07 dispatch instructions classify any provider
  config error as a medium finding.
- **Suggested fix:** ensure `PORTKEY_API_KEY` is exported via
  `~/.env.sh` in the eval environment alongside `OPENAI_API_KEY` and
  `ANTHROPIC_API_KEY`, or document the project-local `settings.toml`
  `[providers.portkey].api_key` entry as a required pre-flight check
  for any wave-2 dispatch.

## Headless visibility limit

The eval harness captures `styled_lines` only for assistant text, not
for the approval menu / preview chrome. The palette findings above
are derived statically by computing luminance against the constants
that `format_approval_menu_lines` and `approval::render_preview` apply
to each span. A `TestBackend`-driven TUI fixture (see
`crates/squeezy-tui/tests/...`) would be needed to capture the
rendered cells directly; that is the wave-1 cancelled-prompt /
restore-on-deny gap noted in
`docs/internal/eval-findings/approval-deny-shell.md` and remains
out-of-scope for headless eval probes.

## How to re-run

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-07-tool-approval-openai.toml --no-triage
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-07-tool-approval-anthropic.toml --no-triage
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-07-tool-approval-portkey.toml --no-triage
```

Expected after the palette + portkey fixes land:

- All three runs complete; `findings.jsonl` empty (the
  `ungrounded_citation` false-positives stay until the rule is
  tightened — separate concern).
- The approval card renders option labels and preview body lines at
  luminance ≤ 160 with GOLD selected-row + AMBER brand cues popping
  out cleanly.
