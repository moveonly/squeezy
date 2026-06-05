# wave2-08-apply-patch-diff-rendering

- **Domain:** 08 — apply-patch diff rendering (approval preview)
- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-08-apply-patch-diff-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-08-apply-patch-diff-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-08-apply-patch-diff-portkey.toml`
- **Run dirs:**
  - OpenAI: `target/eval/wave2-08-apply-patch-diff-openai-1780144006688/` (95 trace events · 1 frame · cost $0.0043)
  - Anthropic: `target/eval/wave2-08-apply-patch-diff-anthropic-1780144068694/` (24 trace events · 1 frame · cost $0.0091)
  - Portkey: `target/eval/wave2-08-apply-patch-diff-portkey-1780144104498/` (provider config error — no run.json)
- **Probe:** the agent was asked to create two new markdown files (`README-PROBE.md`, `README-PROBE2.md`) in a single `apply_patch` call. Both OpenAI gpt-5.4-mini and Anthropic claude-haiku-4-5 routed through the modern `operations: [{kind: create_file, ...}]` payload shape (verified in trace `tool_call_started` for both runs). The approval preview was pre-armed to approve, exercising the `unified_diff` metadata that flows from `crates/squeezy-tools/src/lib.rs:1620` through `crates/squeezy-tui/src/approval.rs:118` to `crates/squeezy-tui/src/render/diff.rs:50` (`render_patch_full_lines`).

## Findings

### F1 — `DIFF_DEL_FG` luminance 191 violates wave-2 dark-only palette ceiling 160 [`squeezy-8dd`]

- **Provider:** all (palette constant, not LLM-dependent)
- **Severity:** P1 (cross-provider, ships in every render of a removed line)
- **Rubric dimension:** 1 (visual clarity / palette guardrails)
- **File:line:** `crates/squeezy-tui/src/render/palette.rs:40`
- **Sibling values for comparison:** `DIFF_ADD_FG` (line 39, `Rgb(21, 128, 61)`) luminance 88.4; `DIFF_HUNK_FG` (line 41, `Rgb(184, 124, 38)`) luminance 132.1 — both inside the dark-mode budget; `DIFF_DEL_FG` `Rgb(252, 165, 165)` computes to `0.299*252 + 0.587*165 + 0.114*165 = 191.0`, well over the 160 ceiling documented in `docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`.
- **Use site:** `crates/squeezy-tui/src/render/diff.rs:244` (`delete_fg_style`), reached for every `-` line in the `/diff` overlay AND every `-` line in the approval-preview diff body (`approval.rs:124 → render_patch_full_lines → render_parsed_lines → render_line`). Result: deletions render brighter than the brand amber, fighting for attention with `AMBER` (luminance 145) on the same frame.
- **Why it slipped past tests:** `crates/squeezy-tui/src/render/diff_tests.rs` asserts the *color is set* and that it differs from the add color — it does not enforce the luminance ceiling.
- **Suggested fix:** drop into the `Rgb(170, 90, 90)` (luminance ~119) neighbourhood so the foreground harmonises with the existing soft-red `diff_del_bg` tint (`crates/squeezy-tui/src/render/diff.rs:263`, dark mode `Rgb(74, 34, 29)`). Add a `#[test]` over each `DIFF_*_FG` and `AMBER`/`GOLD`/`MODE_PURPLE`/`SUCCESS_GREEN`/`MODE_BUILD_GREEN`/`ERROR_RED`/`BANG_RED` constant that fails when luminance > 160.

### F2 — `apply_patch` approval preview misrepresents create/delete/move ops [`squeezy-17m`]

- **Provider:** OpenAI gpt-5.4-mini and Anthropic claude-haiku-4-5 (both used `operations[]`)
- **Severity:** P1 (every `apply_patch` from `operations`-shape calls reaches this code path; reviewer is shown a misleading header)
- **Rubric dimension:** 4 (diff readability)
- **File:line:** `crates/squeezy-tools/src/patch.rs:174` (`render_apply_patch_diff`), with the actual misformatting in `append_create_or_delete_hunk` at line 246 and the `MoveFile` arm at line 211.
- **Evidence:**
  - For each `CreateFile` op, the synthesised approval-preview header is `--- a/<path>\n+++ b/<path>\n@@ -1 +1 @@` instead of `--- /dev/null\n+++ b/<path>\n@@ -0,0 +1,N @@`. The reviewer sees gutter numbers starting at `1` on the `-` side too, so a brand-new file is visually indistinguishable from an in-place edit. Compare the `build_unified_diff` path in the same file at lines 956 / 1004 (`append_file_unified_diff`) which *does* swap to `/dev/null` for create / delete.
  - For each `DeleteFile` op, the same generic header is emitted; the new side should be `/dev/null`.
  - For each `MoveFile` op (line 211) the function emits one header line and no body — so the approval preview for a rename shows literally nothing for that operation.
- **Cross-check:** the *post-apply* result in the same call carries a correct unified-diff (`trace.jsonl` seq 89 in the OpenAI run shows `--- /dev/null\n+++ b/crates/squeezy-eval/README-PROBE.md\n@@ -0,0 +1 @@\n+# Probe one`). The bug is local to the **pre-approval synthesis path** used to render the preview.
- **Knock-on:** because `crates/squeezy-tui/src/render/diff.rs:299` `is_diff_metadata_line` filters out `---`/`+++` lines before display, **the multi-file approval preview never renders a per-file delimiter at all**. In our run the user sees `+# Probe one` followed immediately by `+# Probe two` with no visual indication that two distinct files are being created. (Suggested fix: have `render_apply_patch_diff` emit a `@@` hunk header per file with a path hint, e.g. `@@ create crates/squeezy-eval/README-PROBE.md @@`, so the diff parser picks it up as a `DiffLineKind::Hunk` and the renderer shows a file divider.)

### F3 — `apply_patch` approval summary + paths metadata ignore `operations[]` shape [`squeezy-qkt`]

- **Provider:** OpenAI gpt-5.4-mini and Anthropic claude-haiku-4-5 (both used `operations[]`)
- **Severity:** P1 (defeats the audit goal of the approval modal)
- **Rubric dimension:** 4 (diff readability) + 3 (messaging)
- **File:line:**
  - `crates/squeezy-tools/src/lib.rs:1620-1656` — the `paths` Vec for the approval metadata is built only from `args.patches[*].path`; `operations[]` is ignored, so `paths` ends up empty → metadata `paths: "*"` and `target: workspace:patches`.
  - `crates/squeezy-tools/src/lib.rs:2040-2054` — `format!("apply_patch paths={paths:?}")` for the approval *summary* line builds the same way; empty falls through to `"?"`.
- **Evidence:** `replay.tui` in both successful runs shows the literal lines `approval apply_patch paths="?" -> approved` and `paths: *`. The OpenAI run's argument trace at `tool_call_started` is `{"operations": [{"contents": "# Probe one\n", "kind": "create_file", "path": "crates/squeezy-eval/README-PROBE.md"}, ...]}` — the data is right there, the code just doesn't read it.
- **Suggested fix:** both call sites iterate `args.patches` ⨯ `args.operations` (handling each `ApplyPatchOperation` variant for the primary path) the same way `render_apply_patch_diff` already does at line 191. The `target` should become `path:<that>` for a single-op call to match the legacy single-patch behaviour.

### F4 — Portkey scenario blocked by missing API key [`squeezy-vsx`]

- **Provider:** Portkey (Qwen3 via OpenRouter)
- **Severity:** medium (per wave-2 hard rule for provider config errors)
- **Rubric dimension:** 6 (cross-model consistency — can't be measured for Qwen on this domain without the key)
- **Failure mode:** `provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add [providers.<name>] api_key = "…" to ~/.squeezy/settings.toml or the project-local settings.toml`. Per task hard rule (no settings/env API-key files read) the failure is recorded but not remediated here. Run dir contains `trace.jsonl`, `frames.jsonl`, `replay.tui` from the harness shutdown but no `run.json`.
- **Suggested follow-up:** once Portkey credentials are wired into the dispatch env, re-run the Portkey scenario to confirm F1–F3 also reproduce on Qwen and to evaluate any provider-specific tool-routing differences (Qwen historically lands `tool_choice = "required"` + an `operations[]` payload, same as OpenAI/Anthropic in this run).

## Repro

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-08-apply-patch-diff-openai.toml \
  --no-triage
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-08-apply-patch-diff-anthropic.toml \
  --no-triage
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-08-apply-patch-diff-portkey.toml \
  --no-triage   # requires PORTKEY_API_KEY
```

Inspect `target/eval/wave2-08-apply-patch-diff-<provider>-<unix-ms>/replay.tui`
(the `Overlay state` block carries the approval-event capture) and the
post-apply `tool_call_completed.unified_diff` in `trace.jsonl` (sequence 89
in the OpenAI run) for the contrast between the broken approval-preview
synthesis and the correct post-apply diff blob.

## Harness gap noted (not a defect; future-wave follow-up)

The `tui_capture` mode renders an `Overlay state` markdown summary of the
approval event (`crates/squeezy-eval/src/tui_capture.rs:404`) rather than
the live approval modal that `crates/squeezy-tui/src/lib.rs:4454`
(`format_approval_menu_lines`) would draw on a real terminal. As a result
the captured cell grid has no `+` / `-` / hunk colors to inspect for the
palette violation in F1 — the assessment falls back to inspecting the
palette constant directly. The findings still stand because the constant
flows unmodified to the live render; recommend a follow-up scenario that
sets `drive_tui = true` and captures the styled approval frame so the
luminance assertion can run on the actual rendered cells.
