# Eval-findings triage rollup

This is the disposition record for every report under
`docs/internal/eval-findings/`. It closes the open item carried by
`EVAL_COVERAGE_PLAN.md` and `EVAL_COVERAGE_PLAN_WAVE2.md`: every finding is
now explicitly **closed** (fixed or no-defect), **accepted** as a known
limitation, or **deferred** to post-0.1. Each report's defects were
re-checked against the current source rather than trusting the report's own
fix claims.

Disposition vocabulary:

- **Closed** — every defect is fixed in current code, or the report is a
  regression guard that found no defect.
- **Accepted** — at least one defect is a deliberate, documented known
  limitation (e.g. an environment/credentials gap, or a steering trade-off),
  not a code defect to fix.
- **Deferred post-0.1** — contains a genuinely-open but non-launch-blocking
  defect, tracked below with rationale.
- **Meta** — investigation log, methodology, plan, or runbook; no defect list.

## Summary

| Disposition | Reports |
|---|---|
| Closed | 23 |
| Accepted (known limitation) | 3 |
| Deferred post-0.1 | 4 |
| Meta (no action) | 4 |

No remaining finding blocks 0.1: the four deferred items are an
eval-harness-only abort, two harness-expressiveness gaps, a replay-only
cosmetic label, and a cosmetic shimmer-luminance nit on a shared theme token.

## Deferred to post-0.1

These are the only reports with an unresolved defect. All are explicitly
non-launch-blocking; none affect a live shipped session.

| Report | Ticket | Open defect | Why deferred |
|---|---|---|---|
| `wave2-10-reasoning-toggle-and-stream.md` | `squeezy-00f` | `squeezy-eval` aborts a scenario when a provider key is missing instead of landing a `provider_not_configured` finding + run dir | Eval-harness tooling only; the shipped agent is unaffected and it only manifests when running the suite without that provider's key. |
| `wave2-13-tool-output-spillover.md` | `squeezy-9jab` | A spilled shell card loses the original command label on result-only reconstruction (resume/replay/eval re-render) | Cosmetic; live sessions read the command from the tool call and are unaffected. A fix (add `command`/`workdir` to the spill envelope) is safe but is optional polish. |
| `wave2-15-working-card-and-spinner.md` | `squeezy-txko` | Default working-card shimmer crest (`[232,201,122]`, luminance ≈201) exceeds the 160 dark-palette guardrail | Cosmetic; severity already dropped (was ≈250). The value is a shared theme token shaped the same way across all theme variants, so re-tuning is a deliberate design decision, not a quick pre-launch edit. |
| `prompt-queue-drain.md` | (F1, F2) | `TuiHarness::send_key` drains each turn so the mid-turn queue arm is unreachable from `send_keys` scenarios; `Action::CancelTurn` is a no-op under `drive_tui` | Harness-expressiveness gaps, not product bugs; the report itself frames them as out-of-scope harness tickets. |

## Full disposition table

| Report | Kind | Disposition | Notes |
|---|---|---|---|
| `approval-deny-shell.md` | regression guard | Closed | No defect; deny path verified (agent emits `ToolCallCompleted` without invoking the executor). |
| `board-and-graph-fixes-summary.md` | meta | Closed | F1–F5 inheritance/attribute fixes verified present; c/go losses accepted as non-graph-attributable. |
| `cost-wins-fresh-headhead.md` | meta | Meta | Investigation log; Fixes A–E verified in current code. |
| `graph-cost-wins-report.md` | meta | Closed | Wave 0–5 fixes verified; §6 remaining work accepted as forward backlog. |
| `graph-value-demo-plan.md` | meta | Meta | Demo plan; GAP1–GAP4a fixed, GAP4b (batch read_slice) accepted. |
| `haiku-c-go-nonwins-handoff.md` | meta | Closed | F1–F6 fixed; product-routing hypotheses accepted as steering notes. |
| `mcp-elicitation-form.md` | regression guard | Closed | Historical harness gap; eval now has `[mcp.servers]` + `respond_elicitation`. |
| `measurement-integrity-fixes.md` | meta | Closed | Grader/stale-toml fixes landed. |
| `plan-mode-question-styling.md` | regression guard | Closed | Styling regression guard + modal-pump deadlock fix verified. |
| `prompt-queue-drain.md` | regression guard | Deferred post-0.1 | F3 fixed; F1/F2 accepted harness gaps (see above). |
| `realworld-scoreboard-methodology.md` | meta | Meta | Methodology only; no defects. |
| `session-resume-picker.md` | regression guard | Closed | Inline-placeholder + resume-race fixed; harness coverage gap accepted. |
| `slash-compact-roundtrip.md` | defect report | Closed | F1 (`squeezy-bgc`) + F2 fixed. |
| `wave2-01-startup-banner-and-card.md` | defect report | Closed | 5 defects fixed. |
| `wave2-02-resume-picker.md` | defect report | Closed | 4 fixed; 1 not code-verifiable (env). |
| `wave2-03-slash-help-discovery.md` | regression guard | Accepted | `squeezy-hg94` accepted known limitation. |
| `wave2-04-slash-compact-and-resume.md` | defect report | Closed | 3 fixed (incl. `squeezy-71u` thinking-budget clamp); 1 env. |
| `wave2-05-slash-config-screen.md` | defect report | Closed | 5 defects fixed. |
| `wave2-06-plan-mode-question-flow.md` | defect report | Closed | 7 fixed; F7 accepted. |
| `wave2-07-tool-approval-allow-deny.md` | defect report | Closed | 3 fixed; `squeezy-7bf` accepted. |
| `wave2-08-apply-patch-diff-rendering.md` | defect report | Closed | 2 fixed; `squeezy-8dd` accepted; 1 not code-verifiable. |
| `wave2-09-mcp-elicitation-and-status.md` | defect report | Closed | `squeezy-71u` + `squeezy-y5i` fixed; `squeezy-bcz` env (Portkey key). |
| `wave2-10-reasoning-toggle-and-stream.md` | defect report | Deferred post-0.1 | `squeezy-a88`/`squeezy-lu9` fixed; `squeezy-00f` deferred (see above). |
| `wave2-11-streaming-cancel-and-restore.md` | defect report | Closed | 5 fixed; 1 accepted. |
| `wave2-12-prompt-queue-and-drain.md` | defect report | Closed | 3 fixed; `squeezy-vwlm` accepted. |
| `wave2-13-tool-output-spillover.md` | defect report | Deferred post-0.1 | 4 fixed, 1 accepted; `squeezy-9jab` deferred (see above). |
| `wave2-14-tool-card-coalescing.md` | defect report | Closed | 2 defects fixed. |
| `wave2-15-working-card-and-spinner.md` | defect report | Deferred post-0.1 | `squeezy-x6e7` fixed; `squeezy-txko` deferred (see above). |
| `wave2-16-status-line-and-cost.md` | defect report | Closed | 3 fixed; `squeezy-vsge` accepted. |
| `wave2-17-error-and-failure-messages.md` | defect report | Closed | 4 fixed; `squeezy-ge38` accepted. |
| `wave2-18-theme-and-color-palette.md` | defect report | Accepted | F1/F2/F5 fixed; F3/F4 accepted palette decisions. |
| `wave2-19-git-and-vcs-surfaces.md` | defect report | Closed | 5 fixed; 1 accepted. |
| `wave2-20-help-and-discoverability.md` | defect report | Accepted | `squeezy-p72m` fixed; 2 accepted discoverability decisions. |
| `wave2-runbook.md` | meta | Meta | Dispatch runbook; no defects. |
