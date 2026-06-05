# Eval coverage plan — wave 2 (20 domains × 3 providers)

Status: historical dispatch board for the 2026-05-30 wave-2 bug hunt. The
provider failures and findings below are snapshot evidence, not the current
source of truth for whether a bug is still reproducible.

Each domain gets a single agent that drives the `squeezy-eval` harness
against the squeezy repo itself using **three live provider scenarios**
(one per provider) plus a Beads ticket per defect surfaced.

## Providers in rotation

Each agent authors three live scenarios per domain — one per provider —
so we can compare model behaviour on the same probe shape. The provider
keys must already be configured before dispatch:

- `openai` — model `gpt-5.4-mini`, requires `OPENAI_API_KEY` env.
- `anthropic` — model `claude-haiku-4-5-20251001`, requires
  `ANTHROPIC_API_KEY` env.
- `portkey` — model `@openrouter/qwen/qwen3.6-35b-a3b`, requires
  `[providers.portkey].api_key` in `~/.squeezy/settings.toml`.

All scenarios use `[workspace] local = "."`, `snapshot = true`, so the
agent reads a per-run git worktree of the source tree and any edits it
attempts never touch the live checkout.

## Palette guardrails (read these before evaluating "Visual clarity")

The agent's brand colour is **dark amber `Rgb(215, 147, 52)`**
(`palette::AMBER`). It is reserved for "this is squeezy" cues: banner,
working-card rotating balls / spinner, transcript chevrons, prompt
activity ring, and similar significant brand touchpoints.

All other identifying cues must be **dark, never bright**:

| Semantic | Constant | RGB | Where it belongs |
|---|---|---|---|
| Brand / significant | `AMBER` | 215, 147, 52 | banner, balls, spinner, prompt ring |
| Selected highlight | `GOLD` | 184, 124, 38 | selected list items, focused buttons |
| Question / neutral / plan-mode | `MODE_PURPLE` | 145, 132, 113 | request_user_input modal, plan-mode chrome, config "env" hints |
| Success | `SUCCESS_GREEN` | 22, 101, 52 | turn completed, apply succeeded, all-good toasts |
| Build mode | `MODE_BUILD_GREEN` | 34, 117, 64 | build-mode indicator |
| Failure (recoverable) | `ERROR_RED` | 180, 60, 60 | tool errors, retried turns, denied approvals |
| Failure (hard / bang) | `BANG_RED` | 153, 27, 27 | `!`-shell prompts, fatal toasts |
| Quiet chrome | `QUIET` | `DarkGray` | hints, secondary labels, hint rules |

Rule of thumb: any RGB whose luminance
`0.299*R + 0.587*G + 0.114*B` exceeds ~160 is too bright and is a
finding regardless of where it appears in the UI. Bright accents make
the brand amber indistinguishable from the semantic markers and
exhaust the dark-mode user.

## Evaluation rubric

Every probe asks the same six questions of the resulting transcript /
trace / frame and surfaces each as a separate finding when it fails:

1. **Visual clarity** — does the surface read cleanly? Distinct colours
   for question vs. answer vs. status? Is the rendered frame
   skim-readable in one second? **Any cell rendering with a luminance
   > 160 RGB is a finding** (see palette guardrails above); cite the
   offending constant or hard-coded RGB.
2. **Functionality** — does the surface do what its name promises? Does
   the happy path complete without manual intervention?
3. **Messaging** — are the human-readable strings concrete,
   actionable, and free of jargon? Do error messages cite a next step?
4. **Diff readability** — when a diff renders (apply_patch, /diff,
   approval preview), can the user scan it without re-reading? Are the
   `+`/`-` markers, file headers, and gutter consistent?
5. **Progressive disclosure** — is the high-level summary always
   visible, with detail one keystroke away? No surprises hidden inside
   collapsed regions, but no walls of text on the default frame.
6. **Cross-model consistency** — does the surface behave the same way
   across the three providers? If a provider triggers a regression the
   other two do not, that is the finding.

## Domain matrix

| # | Domain | Probe focus | Suggested scenario shape |
|--:|---|---|---|
| 01 | startup-banner-and-card | Banner card legibility, language summary accuracy, theme application on first frame, post-#154 styling | `tui_capture` with no prompt (read the first frame only) |
| 02 | resume-picker-and-restore | Picker rows, recency labels, branch tip indicator, Resume → first interactive frame | TUI capture; seed two sessions, then drive picker via `send_key` |
| 03 | slash-help-discovery | `/help`, `/?`, slash menu visibility, fuzzy-match ranking | Mock + live probe of `/help` and `/help <head>` |
| 04 | slash-compact-and-resume | Manual `/compact` mid-conversation, then resume — already known broken (wave-1) | Cross-provider repro of the wave-1 critical |
| 05 | slash-config-screen | `/config` overlay → toggles → save → effect on next render | `slash_command` + `send_keys` + `tui_frame_contains` |
| 06 | plan-mode-question-flow | `request_user_input` modal across all three providers | TUI capture in plan mode, scripted answers |
| 07 | tool-approval-allow-deny | apply_patch + shell approvals; Allow / Deny / Allow-once / Always paths | live providers, `permission_mode = "ask"`, queued approve/deny actions |
| 08 | apply-patch-diff-rendering | Approval preview gutter, syntax-highlighted hunks, multi-file patches | live request for a multi-file edit, capture the approval frame |
| 09 | mcp-elicitation-and-status | MCP request lifecycle, status line copy, elicitation modal | live with a sample MCP server stubbed or `mock` + harness gap from wave-1 |
| 10 | reasoning-toggle-and-stream | Reasoning expand (Ctrl+O/E), partial-stream wrap, hide reasoning toggle | live probe (Anthropic Haiku reasons by default) |
| 11 | streaming-cancel-and-restore | Esc mid-stream, `cancelled_prompt`, Ctrl+R restore | live + scripted cancel via `cancel_turn` |
| 12 | prompt-queue-and-drain | Shift+Enter queue, status bar count, cancel-mid-queue | builds on wave-1 limitations note |
| 13 | tool-output-spillover | Large grep/shell output → preview + spill file + recovery surface | live with a large-output prompt |
| 14 | tool-card-coalescing | Five same-tool calls in one turn collapse into a single card (#145) | live with scripted shell loops |
| 15 | working-card-and-spinner | Working card detail rows, per-tool elapsed, shimmer animation | live; assert via `tui_frame_contains` |
| 16 | status-line-and-cost | Status-line items render in order, cost cap warning surfaces | live; small cost cap to trip warning |
| 17 | error-and-failure-messages | Provider 4xx/429/timeout, tool errors, missing files — readability | scripted via mock + one live overload probe (Anthropic short context) |
| 18 | theme-and-color-palette | `/theme` swap mid-session, AMBER/GOLD/MODE_PURPLE/QUIET coverage | live; `slash_command = "/theme dark"` then assert frame contains expected colour markers |
| 19 | git-and-vcs-surfaces | `/diff`, `/undo`, undo card, conflict messaging | live with a snapshot workspace + scripted edit |
| 20 | help-and-discoverability | First-run hints, keymap discoverability, footer/header copy | live, low-temperature, ask the model where to find feature X — does the agent answer correctly? |

## Per-domain deliverables

For each domain, the assigned agent must:

1. Write three live scenarios (one per provider) under
   `crates/squeezy-eval/fixtures/scenarios/wave2-<NN>-<domain>-openai.toml`,
   `...-anthropic.toml`, `...-portkey.toml`.
2. Run them in order: openai → anthropic → portkey, with
   `--no-triage`. Capture each run directory.
3. For every finding above severity = `none`, file a Beads ticket via
   `bd create --type bug --priority <P0..P3> --title "<headline>"
   --description "<rubric dimension + evidence>"` and record the
   returned ticket id.
4. Write `docs/internal/eval-findings/wave2-<NN>-<domain>.md` with one
   subsection per finding using the template in
   `docs/internal/EVAL_COVERAGE_PLAN.md`. Cite trace lines and ticket
   ids.

## Triage rules

- A finding requires evidence (a `trace.jsonl` seq + `frames.jsonl`
  line or a rendered-frame excerpt). No vibes-only tickets.
- Cross-provider regressions (one provider misbehaves, the other two
  do not) get severity `medium` minimum; cite which provider tripped.
- Findings the agent cannot reproduce on a re-run get severity `low`
  with `[flaky]` in the headline.
- "Looks bad" is not a finding without a specific rubric dimension —
  cite which of the six it violates.

## Triage workflow downstream of this PR

Wave-2 only files tickets and ships reproducer fixtures. Actual
remediation lands in follow-up PRs that cite each ticket id.
