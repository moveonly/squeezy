# wave2-19 git-and-vcs-surfaces

- **Domain:** 19 / `git-and-vcs-surfaces`
- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-19-git-vcs-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-19-git-vcs-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-19-git-vcs-portkey.toml`
- **Run dirs:**
  - `target/eval/wave2-19-git-vcs-openai-1780145631934/`
  - `target/eval/wave2-19-git-vcs-anthropic-1780145684717/`
  - `target/eval/wave2-19-git-vcs-portkey-1780145690164/`
- **Auto-findings:** OpenAI 5 (`approval_unanswered`x2, `slow_first_token`, `denied_tool_call_ux`x2);
  Anthropic 2 (`approval_unanswered`, `denied_tool_call_ux`); PortKey 0 (run aborted).

Status: historical snapshot. Current eval code supports
`[squeezy] checkpoints_enabled = true`, and `drive_tui = true` slash-command
dispatch can exercise TUI-owned surfaces. Preserve the captured findings below
as run evidence, but re-check current `driver.rs` behavior before treating
the overlay or TUI-only harness limitations as open defects.

## Headlines

1. **`track_tree`'s `git add --all -- . :(exclude).squeezy` exits status 1
   when the workspace `.gitignore` matches `.squeezy/`.** Severity: **critical**.
   Blocks every checkpoint-producing edit (write_file, apply_patch,
   notebook_edit) on any squeezy-aware worktree. `bd-squeezy-42pp`.
2. **`/diff` and `/undo` resolve to `DispatchOutcome::TuiOnly`, so the
   eval / RPC drivers cannot exercise the git surface.** Severity:
   **major**. The eval action_step status is `tui_only:diff`
   / `tui_only:undo`, no diff card / rollback runs.
   `bd-squeezy-uh8p`.
3. **`DIFF_DEL_FG = Rgb(252, 165, 165)` luminance 191 exceeds the
   ≤160 dark-only palette rule.** Severity: **major**. Affects every
   removed-line span in the `/diff` card body (and plan-card delete
   preview). `bd-squeezy-sd6v`.
4. **`/undo` on a clean tree renders as red `✖ Failed · no output`
   instead of "nothing to undo".** Severity: **major**. Messaging
   defect — the second `/undo` is success, not failure.
   `bd-squeezy-eol0`.
5. **PortKey provider config missing → scenario aborted.** Severity:
   **medium** (per the wave-2 hard rule). `bd-squeezy-5lo3`.
6. **Historical: `SqueezyOverlay` silently dropped the `checkpoints_enabled`
   scenario key.** Severity: **low**. Scenario-fidelity gap.
   `bd-squeezy-vbta`.

## Reproduction

```sh
source ~/.env.sh
SQUEEZY_CHECKPOINTS_ENABLED=1 cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-19-git-vcs-openai.toml --no-triage
SQUEEZY_CHECKPOINTS_ENABLED=1 cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-19-git-vcs-anthropic.toml --no-triage
SQUEEZY_CHECKPOINTS_ENABLED=1 cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-19-git-vcs-portkey.toml --no-triage
```

Each scenario performs: edit `crates/squeezy-eval/PROBE-19.md` (three
lines) → `/diff` → `/undo` → `/undo` → confirm-existence prompt. The
intent is to surface the `/diff` card colour, the first-undo rollback
card lifecycle, and the second-undo "nothing to undo" messaging.

## Finding 1 — `track_tree` mishandles the `.squeezy/` gitignore advisory (bd-squeezy-42pp)

`crates/squeezy-vcs/src/lib.rs:887-897`:

```rust
let mut add_args = vec![
    "add".to_string(),
    "--all".to_string(),
    "--".to_string(),
    ".".to_string(),
    ":(exclude).squeezy".to_string(),
];
// ...
self.git_vec(add_args)?;          // only accepts exit 0
```

`git_vec` (lib.rs:1619) accepts `&[0]`. When the workspace's
`.gitignore` contains `**/.squeezy/` (the squeezy repo itself ships
exactly that — `.gitignore:24-25`) **and** `.squeezy/` exists at the
workspace root (because squeezy puts its checkpoint shadow there),
git exits status 1 with stderr advisory:

```
The following paths are ignored by one of your .gitignore files:
.squeezy
hint: Use -f if you really want to add them.
hint: "git config advice.addIgnoredFile false"
```

The non-zero exit is what `git_vec` propagates as `SqueezyError::Tool`,
which `write_file` / `apply_patch` / `notebook_edit` (every edit-tool
that calls checkpoint_provider) surfaces as a tool error.

### Evidence

- **OpenAI** (`target/eval/wave2-19-git-vcs-openai-1780145631934/`):
  `trace.jsonl` shows `write_file` returning `status: "Error"` with
  the gitignore advisory body; assistant follows up with `apply_patch`
  (same error), then `shell mkdir -p ... && printf ...` (same error
  but now sourced from the local shell tool's checkpoint guard).
  Run-summary line: `trace: 367 events  frames: 2  tickets: 5
  cost: $0.0237`.
- **Anthropic** (`target/eval/wave2-19-git-vcs-anthropic-1780145684717/`):
  `trace.jsonl` seq 14 carries the exact same advisory body inside
  the `write_file` `tool_call_completed` envelope (status `Error`,
  output_sha256 `71898588…`). Anthropic does not retry — it gives up
  after one tool call and replies "directory appears to be gitignored".

### Deterministic CLI repro

```sh
cd /tmp && rm -rf gtest && mkdir gtest && cd gtest && git init -q
printf '**/.squeezy/\n' > .gitignore
mkdir -p .squeezy/sub && echo shadow > .squeezy/sub/file
echo new > foo.txt
git add --all -- . ':(exclude).squeezy'
echo "exit: $?"   # exits 1, foo.txt is staged anyway
```

### Suspect lines / suggested fix

- `crates/squeezy-vcs/src/lib.rs:897` — call
  `self.git_vec_allow_status(add_args, &[0, 1])` and inspect stderr;
  if every line matches the `addIgnoredFile` advisory, treat as
  success.
- OR pass `-c advice.addIgnoredFile=false` (and the other
  advice toggles) up front so git exits 0 to begin with.
- OR write `.squeezy` into the shadow store's
  `.git/info/exclude` so the user-level `.gitignore` never matches.

Net effect today: any user whose project carries `**/.squeezy/` in
its `.gitignore` (which is the case for the squeezy repo itself) can
never run `/diff` or `/undo` against a meaningful checkpoint — the
edit that should produce the checkpoint always fails. This is the
single biggest reason both live wave-2 runs degraded into a
sequence of no-op slash commands.

## Finding 2 — `/diff` and `/undo` dispatch as `TuiOnly` (bd-squeezy-uh8p)

`crates/squeezy-agent/src/lib.rs:2589-2615`:

```rust
cmd @ (DispatchCommand::Fork
    | DispatchCommand::Resume { .. }
    ...
    | DispatchCommand::Undo            // line 2596
    ...
    | DispatchCommand::Diff            // line 2605
    | DispatchCommand::Feedback { .. }
    ...
    | DispatchCommand::Keymap) => DispatchOutcome::TuiOnly { ... }
```

The real implementations live in `crates/squeezy-tui/src/lib.rs`:
- `handle_slash_diff` at line 5778 (computes the snapshot off the
  blocking pool, pushes a `DiffCardData`).
- `start_local_checkpoint_job` at line 2609 (drives the
  `checkpoint_undo` local tool job via `Agent::start_local_tool_job`,
  which is `&mut self`).

The eval driver routes the dispatch through `Agent::dispatch_command_raw`
(`crates/squeezy-eval/src/driver.rs:819`). For `TuiOnly` the driver
records `action_step.status = "tui_only:diff"` /
`"tui_only:undo"` and moves on; no diff card, no rollback. The
`trace.jsonl` excerpt from the OpenAI run:

```
seq 335: slash_command "/diff"
seq 336: action_step  status="tui_only:diff"
seq 340: slash_command "/undo"
seq 341: action_step  status="tui_only:undo"
seq 345: slash_command "/undo"
seq 346: action_step  status="tui_only:undo"
```

### Suggested fix

- Hoist a thin async helper on `Agent` per command (e.g.
  `Agent::run_diff_snapshot`, `Agent::run_checkpoint_undo`) that
  shells out to the same code path the TUI uses today; have
  `DispatchOutcome` carry the structured result so eval can record
  it and findings rules can audit it.
- At minimum, change `TuiOnly` to be lazy: a callback the TUI
  fulfils and an eval / RPC driver can stub to obtain the same
  payload. Today the entire git-and-vcs surface is dark to anything
  not running ratatui.

## Finding 3 — `DIFF_DEL_FG` violates the dark-only palette (bd-squeezy-sd6v)

`crates/squeezy-tui/src/render/palette.rs:39-41`:

```rust
pub(crate) const DIFF_ADD_FG: Color = Color::Rgb(21, 128, 61);   // luminance 88  ✓
pub(crate) const DIFF_DEL_FG: Color = Color::Rgb(252, 165, 165); // luminance 191 ✗
pub(crate) const DIFF_HUNK_FG: Color = Color::Rgb(184, 124, 38); // luminance 132 ✓
```

Luminance = `0.299·R + 0.587·G + 0.114·B`:
`DIFF_DEL_FG = 0.299·252 + 0.587·165 + 0.114·165 = 75.35 + 96.86 + 18.81 = 190.97`,
well above the ≤160 ceiling from
`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md:27-49`.

`DIFF_DEL_FG` is the foreground every deleted line uses in the
`/diff` card body (`crates/squeezy-tui/src/render/diff.rs:241-247`
`delete_fg_style` adds `Modifier::BOLD`), and in `plan_card.rs:189`
for plan-mode diffs. On ANSI16 terminals `palette::best_color`
quantises to `Color::LightRed` (255,128,128), which is brighter
still.

### Suggested fix

Pick a deeper crimson under luminance 160. e.g. `Color::Rgb(155, 60, 60)`
(luminance ≈ 86) holds the semantic delta against `DIFF_ADD_FG`
without contesting brand AMBER. Update the assertions at
`crates/squeezy-tui/src/lib_tests.rs:3961` and `4177`.

## Finding 4 — `/undo` on a clean tree paints red "Failed · no output" (bd-squeezy-eol0)

The full chain:

1. `crates/squeezy-vcs/src/lib.rs:1029-1046` — when
   `selected_rollback_records` returns empty, `rollback` returns
   `RollbackResult { skipped: true, applied: false, .. }`.
2. `crates/squeezy-tools/src/checkpoints.rs:114-123` —
   `execute_checkpoint_undo` maps that to
   `ToolStatus::Stale` with content `{ "rollback": result }`. No
   `error` / `reason` / `message` string is attached.
3. `crates/squeezy-tui/src/lib.rs:9077` —
   `tool_status_label` renders `Stale` as `"✖ Failed"`.
4. `crates/squeezy-tui/src/lib.rs:9044` — `tool_status_color`
   paints it `ERROR_RED`.
5. `crates/squeezy-tui/src/lib.rs:8908-8967` —
   `tool_result_error_detail` finds no `error`, no `reason`, no
   `exit_code`, no `stderr` / `stdout`, falls through to
   `"no output"`.

User-visible line: `✖ Failed · no output` painted red. For the
documented happy path "I clicked `/undo` twice; the second one had
nothing left to do" this is wrong on all three rubric dimensions:

- **Functionality** (rubric 2): semantically the second `/undo` succeeded — there is nothing to undo.
- **Messaging** (rubric 3): "no output" is not actionable. A
  user reading this thinks the tool crashed.
- **Diff readability** (rubric 4): paints a clean tree red, which
  signals user error where there is none.

### Suggested fix (two layers)

- `crates/squeezy-tools/src/checkpoints.rs:114-123` — when
  `result.skipped && !result.applied`, return
  `ToolStatus::Success` with
  `content: { "rollback": result, "message": "no checkpoint to undo" }`.
- `crates/squeezy-tui/src/lib.rs:8908` — as a defensive fallback,
  surface a top-level `"message"` field before the
  `"no output"` fall-through.

## Finding 5 — PortKey provider config missing (bd-squeezy-5lo3)

Per the wave-2 hard rule, a provider configuration error is a
**medium** finding, not an abort. Recording.

Run dir: `target/eval/wave2-19-git-vcs-portkey-1780145690164/`
(empty `trace.jsonl` and `frames.jsonl`).

Output:

```
squeezy-eval: provider: provider is not configured: missing
PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add
[providers.<name>] api_key = "…" to ~/.squeezy/settings.toml or
the project-local settings.toml
```

Effect: cross-model comparison reduced to OpenAI + Anthropic. The
PortKey-specific risks (Qwen3 `tool_choice = "required"`,
`stop_with_intent_text_no_tool_call`, `finish_reason_not =
["stop_no_action"]`) remain uncovered for wave-2 / domain 19.

Per the rules, we did not read `~/.squeezy/settings.toml` to
confirm the provider block; the harness's own resolution
diagnostic is the source of truth.

## Finding 6 — Historical: `checkpoints_enabled` scenario key was a no-op (bd-squeezy-vbta)

Current code note: `crates/squeezy-eval/src/scenario.rs` now defines
`SqueezyOverlay.checkpoints_enabled`, and `crates/squeezy-eval/src/driver.rs`
threads it into `AppConfig`.

At the time of this run, `crates/squeezy-eval/src/scenario.rs:110-135`
defined `SqueezyOverlay` without a `checkpoints_enabled` field. The
wave-2 git-and-vcs scenarios set `[squeezy] checkpoints_enabled = true`. TOML
deserialization silently drops unknown keys (no
`deny_unknown_fields`), so the flag is a no-op; the operator must
export `SQUEEZY_CHECKPOINTS_ENABLED=1` separately
(undocumented in `docs/internal/EVAL_HARNESS.md`).

### Suggested fix

- Add `pub checkpoints_enabled: Option<bool>` to `SqueezyOverlay`
  and thread it into the resolved `AppConfig` before the run.
- Or remove the misleading key from the three wave-2 scenarios
  (already called out as a no-op in the scenario `description`
  blocks).
- Document `SQUEEZY_CHECKPOINTS_ENABLED=1` in `EVAL_HARNESS.md`
  next to the `OPENAI_API_KEY` quick-start blurb.

## Cross-provider summary

| Provider | Outcome | Notable per-provider behaviour |
|---|---|---|
| OpenAI `gpt-5.4-mini` | completed | Retried write_file → apply_patch → shell mkdir trio after the first gitignore error. Final turn approval-requested `read_file`, driver auto-denied → 2× `approval_unanswered`. |
| Anthropic `claude-haiku-4-5` | completed | Gave up after one `write_file` failure; final turn approval-requested `glob`, driver auto-denied → 1× `approval_unanswered`. |
| PortKey `qwen3.6-35b-a3b` | aborted | Provider config missing — see finding 5. |

`approval_unanswered` is a **pre-existing scenario bug** (the wave-2
scenarios set `permission_mode = "allow"` but do not queue an
`approve` action against the `read_file` / `glob` tools the agent
spawns in the verification turn). Out of domain-19 scope; surfaced
here so the next wave can either rotate `permission_mode = "ask"`
back in with explicit approves, or accept the no-op approval as the
intentional driver default.

## Why no findings.jsonl auto-rule fired

The squeezy-eval rule catalogue (`crates/squeezy-eval/src/findings.rs`)
audits a turn-by-turn signal set: duplicate tool calls, stale
function-call output, high tool burst, approval-unanswered,
stop_with_intent_text_no_tool_call, expect_* soft checks.

None of the wave-2 / domain-19 defects fit any existing rule:

| Defect | Why no rule fired |
|---|---|
| Finding 1 — track_tree gitignore | The tool error is an `Error` `tool_call_completed`. `expect.no_tool_errors = false` (intentionally — we want to capture provider-side recovery), so `expect_no_tool_errors` doesn't fire. No rule audits the body of a tool error against the squeezy `.gitignore` interaction. |
| Finding 2 — TuiOnly slash | `action_step.status = "tui_only:..."` is not a failure signal in the eval schema. The `unsupported_slash_command` rule fires only on `Unsupported` outcome, not `TuiOnly`. |
| Finding 3 — palette luminance | No rule. Visual audits are out-of-band. |
| Finding 4 — clean-tree /undo | `/undo` runs in the TUI process — under eval driver it is `TuiOnly` and never reaches the toolchain. The defect is only visible by source inspection (or by running squeezy interactively). |
| Finding 5 — portkey provider config | Scenario aborted before the rule engine ran. |
| Finding 6 — checkpoints_enabled no-op | Schema gap, not a runtime signal. |

Future work: a `tui_only_action_uncovered` rule (any
`action_step.status` starting with `"tui_only:"` whose command lies
in a configurable allow-list) would at least surface finding 2
automatically.
