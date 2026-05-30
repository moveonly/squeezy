# Slash command & options audit — 2026-05-30

Systematic validation of all 46 slash commands defined in
`crates/squeezy-tui/src/input.rs::SLASH_COMMANDS`, driven via
`squeezy-eval`. Goal was to surface broken / redundant / UX-suboptimal /
unnecessary commands and to identify harness gaps blocking future
testing.

## Method

- Pre-audit coverage: 7 of 46 commands had any eval scenario (15%); 0 of 10 CLI subcommands had eval coverage.
- One small harness primitive added: `Assertion::ModalActive { name }` + `TuiHarness::current_modal` (`crates/squeezy-tui/src/testing.rs`).
- One latent harness bug fixed: when `drive_tui = true`, slash commands now route through the TUI's `handle_slash_command` instead of the agent-only dispatcher; previously TuiOnly commands (`/options`, `/model`, `/theme`, `/effort`, `/keymap`, ...) never reached the TUI from eval. Without this, ~half the slash surface was unreachable from any scenario.
- Four parallel subagents authored TOML scenarios (one + variants per command) and ran them; orchestrator triaged.
- Final corpus: **77 new scenarios** (76 from subagents + 1 smoke) under `crates/squeezy-eval/fixtures/scenarios/audit-*.toml`. All passed.
- Two primitives originally planned (`Assertion::SettingsValue`, `Action::KeyChord`) were dropped after discovering existing facilities cover them: `/effort`-style persistence already sets `app.status` (assertable via `tui_status_contains`); `Action::SendKeys` already pumps between keys.

## Coverage outcome

| Category | Commands | Scenarios | Pass |
|---|---|---|---|
| Config / settings UI | 9 | 17 | 17 |
| Session + checkpoint | 11 | 19 | 19 |
| Plan / context / pin / attach | 16 | 27 | 27 |
| Tasks / aliases / feedback / nav | 10 | 13 | 13 |
| Smoke (harness gate) | — | 1 | 1 |
| **Total** | **46** | **77** | **77** |

CLI subcommands (`config`, `repo`, `sessions`, `feedback`, `mcp`, `ask`, `auth`, `doctor`, `refresh-models`, `providers`) remain out of scope — `squeezy-eval` drives the TUI/agent surface, not the CLI binary. Tracked separately.

## Per-command verdicts

| Command | Verdict | Evidence |
|---|---|---|
| `/options [section]` | working | `audit-config-options-{bare,permissions,models,reset,unknown}.toml` |
| `/model` | working / **alias** | `audit-config-model.toml` — 1-line shortcut for `/options models` |
| `/permissions` | working / **alias** | `audit-config-permissions.toml` — 1-line shortcut for `/options permissions` |
| `/statusline` | working | `audit-config-statusline.toml` (modal not covered by `current_modal` — see harness gap below) |
| `/theme` | working | `audit-config-theme-{dark,unknown}.toml` |
| `/effort` | working | `audit-config-effort-{high,auto,bare}.toml` (bare opens config_screen; with-arg session-scoped) |
| `/verbosity` | working | `audit-config-verbosity-{bare,concise}.toml` (inconsistent surface vs `/effort` — see below) |
| `/tool-verbosity` | working | `audit-config-tool-verbosity-verbose.toml` |
| `/keymap` | working | `audit-config-keymap.toml` |
| `/sessions` | working | `audit-session-sessions-{empty,after-turn}.toml` |
| `/session` | working | `audit-session-session-{missing-id,rename,label}.toml` |
| `/resume` | untestable agent-side | `audit-session-resume-missing.toml` (picker bootstrap requires harness work) |
| `/fork` | working | `audit-session-fork-{bare,tui}.toml` |
| `/session-export` | working | `audit-session-session-export-after-turn.toml` |
| `/session-export-html` | working | `audit-session-export-html-tui.toml` |
| `/session-cleanup` | working | `audit-session-cleanup-{archive,archive-tui,purge-tui}.toml` |
| `/checkpoints` | working | `audit-session-checkpoints-{bare,tui}.toml` |
| `/checkpoint` | untestable agent-side | `audit-session-checkpoint-missing.toml` |
| `/undo` | working | `audit-session-undo-{disabled,enabled-empty}.toml` |
| `/revert-turn` | untestable agent-side | `audit-session-revert-turn.toml` (correctly stays TuiOnly — destructive) |
| `/plan` | working / **partial-bug** | `audit-plan-{bare,with-prompt}.toml` (prompt discarded on agent path) |
| `/build` | working | `audit-build-{bare,noop}.toml` |
| `/plans` | working | `audit-plans-{list-empty,show-missing}.toml` (entirely TUI-side; agent path is TuiOnly) |
| `/cost` | working | `audit-cost{,-after-turn}.toml` |
| `/context` | **feature gap** | `audit-context.toml` — currently mirrors `/cost` (same `session_accounting_snapshot()`); intended spec is consumed/remaining tokens against budget + per-source breakdown (MCP / Skill / internal tools / system / user). See `squeezy-rw0i`. |
| `/reviewer` | working | `audit-reviewer-empty.toml` |
| `/compact` | working / **ux** | `audit-compact-{empty,undo-empty,roundtrip}.toml` — raw error on empty conversation |
| `/collapse` | working / **ux** | `audit-collapse-expand-{tui,categories}.toml` — "0 transcript entries" message awkward |
| `/expand` | working | (same files) |
| `/copy` | working | `audit-copy-{bare,invalid}.toml` |
| `/attach` | working | `audit-attach-{and-list,missing,detach-roundtrip}.toml` |
| `/attachments` | working | `audit-attachments-empty.toml` |
| `/detach` | working | `audit-detach-missing.toml` |
| `/pin` | working | `audit-pin-{tui,unpin-roundtrip}.toml` |
| `/pins` | working | `audit-pins-empty.toml` |
| `/unpin` | working | `audit-unpin-{missing,missing-id}.toml` |
| `/tasks` | working | `audit-misc-tasks.toml` |
| `/task` | working | `audit-misc-task-detail-missing.toml` |
| `/task-cancel` | working | `audit-misc-task-cancel-missing.toml` |
| `/jobs` | **dropped** | was alias for `/tasks`; removed in this audit |
| `/job` | **dropped** | was alias for `/task`; removed in this audit |
| `/job-cancel` | **dropped** | was alias for `/task-cancel`; removed in this audit |
| `/feedback` | working | `audit-misc-feedback-{preview,bare}.toml` |
| `/report` | working | `audit-misc-report-preview.toml` |
| `/help` | working | `audit-misc-help-{bare,topic}.toml` (resolves locally; no provider hits) |
| `/diff` | working | `audit-misc-diff-with-changes.toml` (exposed driver bug, see below) |

## Notable findings

### Bugs

- **B1 — driver `workspace_root_clone` fixed** (`squeezy-nyg8.1`). Now reads `agent.config().workspace_root` instead of `std::env::current_dir()`. `edit_file` actions land in the agent's actual workspace (snapshot worktree when `snapshot = true`); previously they wrote to the host repo (corrupted README.md mid-audit).
- **B2 — `pump_until_idle` now waits for `pending_diff`** (`squeezy-nyg8.2`). The idle return predicate gained an `&& !app.pending_diff.is_some()` guard so `/diff`'s spawn_blocking task can land its result before the next assertion. `audit-misc-diff-with-changes.toml` simplified — no more `wait_seconds = 3` + extra slash workaround.
- **B3 — `/plan <prompt>` divergence fixed** (`squeezy-9n9w`). `DispatchOutcome::ModeChanged` gained `prompt: Option<String>`; the agent dispatcher surfaces the prompt arg through it. Non-TUI callers (RPC, squeezy-eval) can now see the prompt and act on it; the TUI handler keeps reading the prompt off `DispatchCommand::Plan { prompt }` directly. Regression test in `lib_tests.rs::dispatch_command_plan_with_prompt_surfaces_prompt_in_outcome`.
- **B4 — `/compact` empty-conversation graceful no-op** (`squeezy-kkdb`). `compact_context_manual` returns `Result<Option<ContextCompactionReport>>` (`Ok(None)` when nothing is compaction-eligible). `DispatchOutcome::Compacted { skipped }` carries the no-op marker. TUI status line shows "nothing to compact yet" instead of `compact failed: agent error: not enough context to compact`.

### Redundancy

- **R1 — `/jobs`, `/job`, `/job-cancel` dropped** (`squeezy-d0nx`). They were pure aliases for `/tasks`, `/task`, `/task-cancel`; the agent dispatcher's match arms shared branches; alias-parity scenarios confirmed identical outcomes. Per the project's no-deprecation stance, the variants are gone from `DispatchCommand`, the parser, and `SLASH_COMMANDS` in this audit; the alias-parity scenarios are deleted.
- **R2 — `/context` is a feature gap, not redundancy** (`squeezy-rw0i`). Today it calls `Agent::session_accounting_snapshot()` and shows roughly the same data as `/cost`. The intended spec is: tokens consumed / tokens remaining against the context budget, plus a per-source breakdown (MCP / Skill / internal tools / system / user). `/cost` stays the cost-oriented view; `/context` becomes the budget-oriented view with per-source attribution.
- **R3 — `/model` and `/permissions` are 1-line aliases** for `/options models` / `/options permissions`. All three open the same `config_screen` modal (focused on different sections); `modal_active = "config"` for all. Keeping them is reasonable (discoverability), but the dispatch arms in `crates/squeezy-tui/src/lib.rs:2200-2213` could note they're aliases.

### UX inconsistencies

- **U1 — settings commands unified on status line** (`squeezy-a19z`). `/effort`, `/verbosity`, `/tool-verbosity` all now report changes via `app.status` (the immediate-feedback surface). The `app_notifications` (toast) push that `/effort` used to add was removed for consistency — notifications stay reserved for asynchronous / "you might've missed this" messages.
- **U2 — bare settings commands no longer divert to `config_screen`** (`squeezy-3ys0`). `/verbosity` and `/tool-verbosity` now print current value + usage hint into the transcript and update the status line, matching the shape `/effort` had. Mode-switch on argument presence is gone. To open the config section for these settings, use `/options verbosity` / `/options tool-verbosity` explicitly.
- **U3 — `/collapse <category>` empty state** (`squeezy-o3z0`). When no entries match the requested category, the status line now reads `no <category> entries to collapse` (or `no matching entries to collapse` for bare `/collapse`) instead of the awkward `collapsed 0 transcript entries`.

### Dead code

- **D1 — Dead overlay variants dropped** (`squeezy-h2ab`). `Overlay::Permissions`, `Verbosity`, `ToolVerbosity`, their builder helpers, and `overlay_tests.rs` are removed; `Overlay::Model` is the only remaining variant. `/permissions`, `/verbosity`, `/tool-verbosity` all route through `toggle_config_screen` and continue to work unchanged.

## Harness gaps (surfaced by subagents)

- **H1 — `current_modal()` now covers `status_line_setup`** (`squeezy-nq30`). Returns `Some("statusline")` when `/statusline` opens its modal; `audit-config-statusline.toml` was migrated from substring fallback to `modal_active = "statusline"`.
- **H2 — No `config_screen_section { name }` assertion** to disambiguate `/options` vs `/model` vs `/permissions` (which all share `modal_active = "config"`). Today scenarios depend on substring-matching section labels in the rendered frame.
- **H3 — No `dispatch_outcome_contains` / `action_step_status_contains` assertion** to inspect agent-side command status without driving the full TUI. Lifts the typed `DispatchOutcome` value off the trace.
- **H4 — No `capture_session_id` (or template var) action** for chained scenarios that need to reference the agent's current session id. Today only the missing-id error path is testable for `/session-export`, `/checkpoint <id>`, `/resume <id>`.
- **H5 — Driver bug B1** (above) blocks any scenario that combines `[workspace] snapshot = true` with `edit_file`.
- **H6 — `pump_until_idle` bug B2** (above) blocks reliable `/diff` testing.

## Tickets filed

- `squeezy-nyg8` (epic) — Slash command audit · 2026-05-30 follow-ups.
- `squeezy-nyg8.1` (P1, bug) — B1: driver `workspace_root_clone` writes to host workspace.
- `squeezy-nyg8.2` (P2, bug, harness-gap) — B2: `pump_until_idle` doesn't drain `pending_diff`.

Remaining items (B3, B4, R1–R3, U1–U3, D1, H1–H6) are documented in this report and should be filed as individual `bd` children of `squeezy-nyg8` when ready to act on them — the report sections above carry enough detail to copy into ticket bodies verbatim.

## Out-of-scope (separate work)

- 10 CLI subcommands (`config`, `repo`, `sessions`, `feedback`, `mcp`, `ask`, `auth`, `doctor`, `refresh-models`, `providers`) — `squeezy-eval` drives the TUI/agent, not the CLI binary.
- Pre-existing test failure on `audit/slash-commands` branch (also fails on origin/main): `crates/squeezy-tui/src/lib_tests.rs::tui_harness_settings_override_pins_theme_writes_to_scratch`. Not caused by this audit — flagged separately.
