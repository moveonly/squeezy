# Website TUI Workflow Research

Status: current-tree research for website copy. Last checked in this checkout on
2026-06-05. No network sources were used. Do not edit `squeezy-site/` from this
note; treat it as input for later site copy and visual planning.

## Scope Checked

- `crates/squeezy-tui`: startup picker, resume picker, config screen, status
  line, slash menu, prompt queue, plan cards, subagent pane, renderer tests.
- `crates/squeezy-agent`: typed slash dispatch and Plan-mode instructions.
- `crates/squeezy-eval`: TUI capture artifacts, frame rendering, wave-2
  scenario fixtures.
- `docs/internal/eval-findings`: wave-2 findings for startup, resume, slash
  discovery, config, plan-mode questions, prompt queue, tool cards, working
  card, status/cost, help/discoverability.
- `docs/internal/website-research/features.md`: broader product-feature guardrails.

Note: `docs/external/` is not present in this checkout. Several eval findings
refer to external docs such as `docs/external/PROVIDERS.md`, but the checked
user-facing docs appear to live under `crates/squeezy-skills/external-docs/` in
this tree. Do not make a website claim that depends on `docs/external/` until
that layout is reconciled.

## Positioning Summary

The TUI is worth promoting as an operator surface for repeated coding sessions:
setup, session recovery, configuration, cost/context awareness, Plan/Build mode,
queued prompts, and bounded subagent visibility. Keep the language utilitarian.
The strongest product claim is not "visual IDE"; it is that common agent
workflows stay visible and controllable from a terminal.

Public-safe framing:

- "A terminal UI for long coding sessions: configure models, resume work, plan
  before edits, queue follow-up prompts, and keep tool activity visible."
- "Status, cost, context, permissions, and session controls stay close to the
  prompt instead of being hidden in logs."
- "Subagent activity is bounded and reviewable in the TUI; it is not an
  autonomous background fleet."

Avoid:

- "IDE", "dashboard", "guaranteed safe", "perfect restore", "billing accurate",
  or "fully autonomous multi-agent execution".
- Hard claims based only on wave-2 defect reports. The eval findings are useful
  evidence of coverage and design direction, but some entries describe bugs or
  harness gaps.

## Implemented Feature Inventory

| Workflow | Implemented behavior | Public-safe claim | Caveats | Source refs |
| --- | --- | --- | --- | --- |
| First-run setup picker | Startup setup is inside the TUI. It asks for theme first and applies/previews theme before provider/model pages. Steps include provider, optional provider-key follow-up, model, and reasoning effort for reasoning-capable models. Custom-theme and missing-key rows can route the user to `/config` after setup. | "First-run setup keeps theme, provider, model, keys, and reasoning effort in the terminal." | Do not imply credentials are stored on the website. Missing provider keys can be deferred to local config. | `crates/squeezy-tui/src/startup_model_picker.rs:1-5`, `:92-110`, `crates/squeezy-tui/src/startup_model_picker_tests.rs:51-73`, `:99-120`, `:192-220` |
| Resume picker | Resume candidates are filtered to recent, resumable, meaningful sessions; capped at 100; rendered newest-first. Rows can prefer user display names, show labels, annotate project/cwd, expand branch tips, and expose cross-project rows after the user opts in. "Start fresh" is the safe default row. | "Resume, label, fork, and continue local sessions without hunting through log files." | The picker is local and depends on session metadata/logs. Current comments describe the picker as part of the resume flow, not a cloud sync feature. | `crates/squeezy-tui/src/resume_picker.rs:1-8`, `:34-42`, `:44-75`, `:116-173`, `crates/squeezy-tui/src/resume_picker_tests.rs:199-252` |
| Slash command discovery | The TUI has a static slash-command catalog with descriptions, parameter hints, availability during active turns, and capability badges such as `read`, `edit`, `net`, `git`, and `destructive`. Commands include help, config/model/permissions/MCP, plan/build/plans, cost/context, attach/pins, sessions/resume/fork/export, checkpoints/undo, effort/verbosity, statusline/theme/spinner/keymap, and routing controls. The agent also has a typed `DispatchCommand` enum for the same command surface. | "Typed slash commands expose common session controls without leaving the terminal." | Some commands are TUI-only overlays. Capability badges are guidance about the command surface, not a complete permission audit. | `crates/squeezy-tui/src/input.rs:11-23`, `:124-177`, `:220-352`, `:371-394`, `crates/squeezy-agent/src/dispatch.rs:1-31`, `:38-175` |
| Local `/help` and fuzzy menu | Wave-2 slash-help probes show curated `/help` and `/help providers` can render local topic answers, while slash autocomplete supports fuzzy matches such as `/com` -> `/compact` and `/atc` -> `/attach`. | "Help and command discovery start locally, so common questions do not have to become model turns." | The eval finding references `docs/external/PROVIDERS.md`, but that path is absent in this checkout. Keep copy generic until docs layout is verified. | `docs/internal/eval-findings/wave2-03-slash-help-discovery.md:17-34`, `:83-111` |
| Full-page config screen | `/config` or F11 opens a full-page config UI with User/Repo/Local tabs, a section sidebar, field editor, footer hints, and save semantics split into Immediate, NextPrompt, and Restart. It also tracks feedback for transcript output, includes reset/undo/discard flows, has provider key rows, theme editing, and live MCP status/actions. | "Edit local, repo, and machine-specific settings from the TUI, with clear apply timing." | Some settings still require restart. Repo/Local path wording must be checked in screenshots to avoid exposing local paths. | `crates/squeezy-tui/src/config_screen.rs:1-8`, `:152-160`, `:193-253`, `crates/squeezy-tui/src/config_screen/render.rs:63-84`, `:86-218` |
| External settings reload | A lightweight watcher polls the three settings tiers and reloads when a settings file appears, disappears, or changes. | "External config edits can be picked up by a running session when the setting supports it." | Polling is local mtime polling, not a remote sync system. | `crates/squeezy-tui/src/settings_watcher.rs:1-12`, `:29-58` |
| Configurable status line | The default status line includes provider/model, current dir, detected languages, git branch/PR/branch changes, and cost. The full item set covers model, path, branch, state, usage, limits, metadata, permission mode, MCP, receipts, pins, compaction, and task progress. `/statusline` opens a checkbox/search/reorder picker with a live preview and theme-color toggle. | "Put model, repo, branch, cost, context, permissions, and MCP state in the status line you actually use." | Cost and token data are estimates or provider-reported counters, not a billing authority. | `crates/squeezy-tui/src/status.rs:21-35`, `:37-88`, `:135-221`, `crates/squeezy-tui/src/status_line_setup.rs:1-12`, `:55-83`, `:158-217` |
| Context and cost commands | `/cost` formats provider/model/mode, estimated USD, provider-reported tokens, tool counts, subagent spend, receipt/spill/I/O metrics, and a clear accuracy caveat. `/context` shows consumed and remaining tokens, headroom, max-output reserve, and source breakdown. | "Inspect cost and context before the session quietly gets expensive." | Use "estimated" for USD. Provider-reported tokens vary by provider. | `crates/squeezy-tui/src/commands.rs:40-175`, `:177-260` |
| Plan mode and plan artifacts | Plan mode appends a compact instruction overlay that tells the model it has Read/Search only, asks it to ground in code before questions, and requires a final `<proposed_plan>` block. Proposed plans are extracted from the stream, persisted under `.squeezy/plans/<session>/`, tracked by a `current` pointer, retained with limits, and rendered as calm plan sections from disk. Build mode can carry a compact "plan still in effect" marker instead of re-paying the full plan every turn. | "Plan mode creates a persisted plan before edits, then Build mode can carry that plan forward." | Do not imply Plan mode can execute changes. It is explicitly read/search oriented except tightly scoped active-plan refinement. | `crates/squeezy-agent/src/plan_mode.rs:1-18`, `:44-49`, `:62-99`, `crates/squeezy-tui/src/proposed_plan.rs:1-12`, `:25-38`, `:45-69`, `crates/squeezy-tui/src/render/plan_card.rs:1-18`, `:48-70`, `:105-123` |
| Plan-mode question flow | The TUI has a request-user-input modal path with choice/freeform hints; wave-2 found validation and palette issues, but the product surface is real and tied to Plan mode. | "When a planning turn needs a decision, the TUI can ask a focused question instead of burying choices in prose." | Do not claim the modal validation bugs are fixed from this doc alone. Verify current code/tests before public screenshots. | `docs/internal/eval-findings/wave2-06-plan-mode-question-flow.md:1-23`, `:36-72` |
| Prompt queue | While a turn is running, Enter and paste can enqueue composer text instead of rejecting it. The next queued prompt drains when the active turn finishes. The overlay supports select, Shift+Up/Down reorder, Delete removal, and a one-line `queued: N` indicator. | "Keep working while the model is running: queue follow-up prompts and reorder them before they drain." | Eval notes say the current live harness had trouble exercising the mid-turn queue path; source and unit tests should be the public claim basis. | `crates/squeezy-tui/src/prompt_queue.rs:1-10`, `:42-58`, `:131-190`, `:196-220`, `docs/internal/eval-findings/wave2-12-prompt-queue-and-drain.md:1-21` |
| Subagent pane | The TUI stores subagent lifecycle records with prompt, latest activity, metrics, and bounded transcript. The pane renders `main` plus subagent rows, shows running/done/failed/capped states, scrolls overflow, collapses to a one-line summary when all subagents finish, and opens full subagent transcripts from the pane. Finished subagents can be cleared while running rows are kept. | "Parallel exploration stays visible: see each subagent's state, switch into its transcript, then clear finished work." | Say "bounded, session-local subagents", not "autonomous workers". Subagents are designed as scoped helpers. | `crates/squeezy-tui/src/lib.rs:13986-14009`, `:14011-14070`, `:14072-14254`, `:15022-15095`, `:16041-16228`, `:16267-16305`, `crates/squeezy-tui/src/lib_tests.rs:231-258`, `:382-406`, `:629-671` |
| Tool, approval, and working surfaces | The TUI keeps active work, approval prompts, tool results, grouped/coalesced tool cards, and full-transcript expansion close to the prompt. Wave-2 findings explicitly probed working card, tool coalescing, approval, and status/cost domains. | "Tool activity is summarized in the transcript, with approval and full-transcript affordances when detail matters." | Some wave-2 docs describe defects or harness gaps. Use them as coverage evidence, not as proof that every visual issue is resolved. | `docs/internal/eval-findings/wave2-14-tool-card-coalescing.md:1-22`, `docs/internal/eval-findings/wave2-15-working-card-and-spinner.md:1-43`, `docs/internal/eval-findings/wave2-16-status-line-and-cost.md:1-28` |
| Eval TUI captures | `squeezy-eval` can write `frames_tui.jsonl` with cell grid, plain text, ANSI replay, style metadata, overlay events, status text, transcript summaries, and `replay.tui`. `frames.jsonl` also stores TUI-rendered markdown spans and ANSI. | "The product has a local eval path for inspecting what the TUI rendered, not just what the model said." | Eval capture is an internal validation claim unless a public docs page explains it. Some drive-TUI paths had historical frame gaps. | `crates/squeezy-eval/src/tui_capture.rs:1-22`, `:42-103`, `:174-240`, `crates/squeezy-eval/src/frames.rs:13-58`, `:100-127`, `docs/internal/EVAL_HARNESS.md:1-23` |

## Visual And Screenshot Ideas

No durable product screenshots were found in the repo outside `squeezy-site`
favicon/OG SVG assets and many ignored or generated site copies. For website
visuals, generate fresh, sanitized captures from the TUI or the eval harness.

Good first visuals:

1. First-run setup sequence:
   - Frame 1: "Choose a theme".
   - Frame 2: provider/model choice.
   - Frame 3: reasoning effort.
   - Caption: "Setup stays inside the terminal."

2. Resume picker:
   - Show `Start fresh` selected, two recent sessions with labels, one branch
     tip row, and a cross-project row marker.
   - Caption: "Resume a local session, or start clean."

3. Slash menu:
   - Type `/con` or `/com` so fuzzy suggestions and capability badges appear.
   - Caption: "Common controls are typed commands, not hidden flags."

4. Config screen:
   - Use a sanitized home path and a repo path that does not reveal private
     directories.
   - Show User/Repo/Local tabs, section sidebar, and one editable field.
   - Caption: "Layered config without leaving the TUI."

5. Status line setup:
   - Show search/filter, checkboxes, Shift+Up/Down reorder hint, and live
     preview.
   - Caption: "Choose what the status line should spend attention on."

6. Plan mode:
   - Show a persisted plan section with `Plan plan-...` and a short numbered
     plan body.
   - Optional adjacent frame: post-plan prompt to switch to Build mode.
   - Caption: "Plan first, then carry the plan into Build mode."

7. Prompt queue:
   - While a turn is running, show `queued: 2` and the reorder overlay.
   - Caption: "Queue follow-ups while the current turn runs."

8. Subagent pane:
   - Show `main`, `delegate #1 running`, `explore #2 done`, and one failed or
     capped row only if the copy discusses limits honestly.
   - Use the full transcript overlay for one selected subagent as a second
     frame.
   - Caption: "Subagent work stays inspectable."

9. Working/tool surface:
   - Show active working row, a summarized tool result, and the hint for the
     full transcript.
   - Caption: "Tool calls stay visible without flooding the main prompt."

Capture hygiene:

- Redact local usernames, API key env names only when needed, absolute private
  paths, session ids, and costs from real runs.
- Prefer synthetic fixture workspaces with public paths like
  `/work/squeezy-demo`.
- Avoid screenshots that show historical wave-2 defects such as raw provider
  JSON errors or palette findings unless the page is explicitly about QA.
- Do not use eval run directories in copy; they are internal evidence, not user
  artifacts.

## Copy Ideas

Homepage feature card:

- Title: "Terminal workflows for real coding sessions"
- Body: "Configure models, resume local sessions, plan before edits, queue
  follow-ups, and keep cost/context visible from the same prompt."

TUI section headline:

- "The terminal stays in the loop"
- "Squeezy keeps setup, planning, approvals, cost, and session recovery close
  to the code you are working on."

Short bullets:

- "First-run setup for theme, provider, model, keys, and reasoning effort."
- "Resume, label, fork, and export local sessions."
- "Use Plan mode to create a persisted plan before Build mode edits files."
- "Queue follow-up prompts while a turn is still running."
- "Open `/config` for layered User, Repo, and Local settings."
- "Customize the status line for model, repo, branch, cost, context, MCP, and
  permission state."
- "Inspect bounded subagent work without mixing every subagent transcript into
  the main conversation."

Screenshot captions:

- "Choose a model once, then keep the session moving."
- "The status line shows the parts of the session you care about."
- "Plan mode writes a durable plan before code changes begin."
- "Queued prompts drain in order after the active turn completes."
- "Subagents are visible as rows, with full transcripts one key away."

Longer product paragraph:

Squeezy's TUI is built for repeated agent work in a terminal. It handles first
run setup, local session recovery, typed slash commands, layered configuration,
Plan/Build mode, prompt queueing, subagent visibility, and cost/context
accounting. The goal is not to replace an editor; it is to keep the agent's
state, permissions, spend, and next actions visible while the editor remains
where code review happens.

## Public Claim Guardrails

Use:

- "local sessions", "resumable", "reviewable", "bounded", "visible",
  "configurable", "estimated cost", "provider-reported tokens".
- "Plan mode creates a persisted plan before edits."
- "Subagent activity stays visible in a bounded TUI pane."
- "Status and slash commands expose controls without leaving the terminal."

Avoid:

- "Cloud sync", "cross-device resume", "perfect replay", or "guaranteed
  rollback" for sessions.
- "Billing accurate" for `/cost`.
- "Autonomous agents" for subagents.
- "IDE-grade UI" or "visual editor".
- "Fully verified across every provider" unless current eval runs are rerun
  and clean.

## Follow-Up Before Site Use

- Generate fresh screenshots or eval captures from the current binary; do not
  reuse historical wave-2 defect previews.
- Verify current palette before public screenshots because several wave-2 docs
  were written as palette audits.
- Decide whether to mention eval capture publicly. It is useful validation
  infrastructure, but it may belong on docs/internal or an engineering blog
  rather than the landing page.
- Reconcile `docs/external/` references. This checkout does not contain that
  directory, while eval notes still reference it.
- Keep any public TUI section secondary to Squeezy's core positioning: local
  semantic navigation before spending model tokens.
