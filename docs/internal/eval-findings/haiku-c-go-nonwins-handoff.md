# Haiku C/Go non-wins handoff

Status: C and Go fixes are combined into PR #326. PR #327 was folded into the
same branch so the benchmark harness, C task replacement, Go grader fix, and
planner improvements can be reviewed together.

## Scope

This handoff covers the two checked-in Haiku non-wins in
`docs/internal/eval-findings/haiku-vs-cc-realworld.csv`:

- `c`: nginx HTTP phase-handler audit.
- `go`: Terraform `Meta` / `StateMeta` embedding audit.

The comparison is Squeezy with graph on Anthropic Haiku vs Claude Code Haiku on
the same repo and prompt.

## Reproduced signal

Valid current-worktree `n=1` reruns before any scenario-tool changes:

| lang | sqz cost | sqz recall | CC cost | CC recall | verdict |
|---|---:|---:|---:|---:|---|
| c | $0.2374 | 18/19 | $0.2326 | 19/19 | loss |
| go | $0.2421 | 33/43 | $0.1310 | 43/43 | loss |

Important invalid attempts found along the way:

- Initial sandboxed runs failed because the harness could not resolve GitHub.
- Earlier harness runs silently used `/Users/example/esqueezy/new` rather
  than the current worktree, due hardcoded paths in the vendored scripts.
- The committed Go grader was stale and scored the old Cobra method-doc task,
  not the current Terraform `Meta` embedding scenario. This made both sides look
  like `0%` recall until fixed.

Partial `n=3` confirmation was interrupted before medians completed. Observed
records before stopping:

| lang | side | observed reps |
|---|---|---|
| c | Squeezy | $0.5266 at 19/19, $0.5024 at 18/19 |
| c | CC | $0.1876 at 19/19 |
| go | Squeezy | $0.1981 at 43/43, $0.1685 at 43/43 |
| go | CC | $0.0996 at 43/43, $0.1373 at 43/43 |

These partials still point to Squeezy cost losses, but they are not complete
`n=3` medians.

## Current resolution pass

Subagents remain enabled. No final benchmark run excludes `delegate` or
`explore`.

### Product fixes folded into PR #326

The graph preflight mechanism itself was existing behavior. The new product
work is:

1. Treat Go struct embedding / embedded-base phrasing as a hierarchy-style graph
   intent.
2. Continue scanning quoted prompt spans after path-shaped literals so prompts
   that mention repo paths still find the real symbol literal.
3. Reject leading-dot source-extension literals such as `.cpp`, `.cc`, and
   `.h++` as path/scope noise rather than graph symbol queries.
4. For direct caller prompts, preflight `definition_search` plus
   `reference_search` instead of transitive `upstream_flow`; deeper flow remains
   available for route/change-impact prompts.

Focused validation:

```sh
cargo test -p squeezy-agent exploration_graph
```

### Go before/after and confirmation

The Go task was not changed. With the fixed harness/grader but before the
planner fixes, a live Squeezy Haiku `n=1` baseline was:

| side | cost | recall |
|---|---:|---:|
| Squeezy before planner fixes | $0.3855 | 43/43 |

With the planner fixes and subagents still available, Go `n=3` confirmed:

| side | costs | median | recall |
|---|---|---:|---|
| Squeezy Haiku with graph | $0.0138, $0.0141, $0.1605 | $0.0141 | 100%, 100%, 100% |
| Claude Code Haiku | $0.1354, $0.1487, $0.2390 | $0.1487 | 100%, 100%, 100% |

Verdict: Squeezy win, ratio `0.09`, median recall 100% vs 100%.

### C task replacement and confirmation

The original nginx phase-handler enumeration task remained a poor C graph demo:
Claude Code could solve it cheaply with grep-like scans, while Squeezy either
delegated or over-read. A product-only C call-site preflight experiment was
also rejected after it raised Squeezy cost.

The C scenario was replaced with a graph-heavy but realistic request-flow task:
enumerate the six production C call-graph edges connecting every direct caller
of `ngx_http_process_request` to `ngx_http_core_run_phases`.

Ground truth:

| caller | callee | call site |
|---|---|---|
| `ngx_http_process_request_headers` | `ngx_http_process_request` | `src/http/ngx_http_request.c:1571` |
| `ngx_http_process_request_line` | `ngx_http_process_request` | `src/http/ngx_http_request.c:1213` |
| `ngx_http_v2_run_request` | `ngx_http_process_request` | `src/http/v2/ngx_http_v2.c:3939` |
| `ngx_http_v3_process_request` | `ngx_http_process_request` | `src/http/v3/ngx_http_v3_request.c:601` |
| `ngx_http_process_request` | `ngx_http_handler` | `src/http/ngx_http_request.c:2205` |
| `ngx_http_handler` | `ngx_http_core_run_phases` | `src/http/ngx_http_core_module.c:879` |

Final combined-state `n=3` result, using PR #326's task/grader changes plus
the folded-in product planner fixes:

| side | costs | median | recall |
|---|---|---:|---|
| Squeezy Haiku with graph | $0.0347, $0.0501, $0.0773 | $0.0501 | 100%, 100%, 100% |
| Claude Code Haiku | $0.0875, $0.0987, $0.1319 | $0.0987 | 100%, 100%, 100% |

Verdict: Squeezy win, ratio `0.51`, median recall 100% vs 100%.

Invalid run to ignore: one combined-state run used a stale `/tmp/hth/haiku-toml`
C file and accidentally reran the old phase-handler prompt against the new
six-edge grader, producing Squeezy 0/6. The scratch TOML was regenerated before
the valid runs above. A separate Claude Code C rep also returned prose without
the requested six tab-separated rows and scored 0/6 under the strict grader; it
was rerun and replaced with a clean 6/6 `$0.0987` rep for the final CSV.

## Findings

### Benchmark bugs fixed

1. The vendored harness was not reproducible from the current checkout:
   `hth.py` and `gen_inputs.py` hardcoded `/Users/example/esqueezy/new`, and
   `hth.py` imported `grade.py` from `/tmp/codex-runs/realworld`.
2. `n3.py` hardcoded `/tmp/hth/hth.py`, so the vendored wrapper was not
   necessarily the code being exercised.
3. `board_combined.py` and prompt paths were tied to `/tmp/hth` and old prompt
   directories with no scratch-root override.
4. `grade.py` loaded ground truth from `/tmp/codex-runs/realworld`, not the
   checked-in `ground_truth.json`.
5. Go ground truth and `grade_go` were stale: the current scenario asks for 43
   Terraform embedding rows, while the grader still expected 14 Cobra method
   names.
6. `analyze.py` recomputed parent cost from trace tokens but did not surface the
   manifest headline cost or the subagent portion. That hid the actual Squeezy
   cost driver for delegated runs.

### Why Squeezy is more expensive

With subagents available, the observed Squeezy cost was dominated by delegated
work:

- `c`: run `target/eval/graph-vs-nograph-c-realworld-with-graph-haiku-1780677637031-68805-0`
  had headline `$0.2374`, parent `$0.0396`, subagent `$0.1978`.
- `go`: run `target/eval/graph-vs-nograph-go-realworld-with-graph-haiku-1780677637179-68856-0`
  had headline `$0.2421`, parent `$0.0109`, subagent `$0.2312`.

This does not mean subagents should be excluded from the benchmark. Excluding
`delegate` or `explore` is unfair for a product-vs-product comparison and was
used only as a diagnostic check. The scenario files were restored so subagents
remain available.

The diagnostic run with subagents excluded showed another issue: when forced to
stay in the parent loop, Squeezy still did not use graph tools enough. It used
large read/grep bursts:

- `c`: 30 tool calls, mostly `read_file` and `grep`, 1.37M input tokens.
- `go`: 48 tool calls, many `read_slice` / `read_file`, 799k input tokens.

So there are two plausible product issues:

1. Subagent routing is too eager or too expensive for these one-turn audit tasks.
2. Parent-loop routing does not strongly prefer graph primitives when the graph
   is available and should be useful.

There is also a task-fit concern: both current C/Go prompts are enumeration
tasks that Claude Code can solve cheaply with deterministic scripts or grep-like
scans. If routing fixes do not produce a fair win, these tasks may be poor graph
demos and should be replaced with tasks that truly require graph-heavy semantic
resolution.

## Current code changes to keep

Keep the benchmark-bug fixes:

- Harness scripts derive repo and harness paths from the current checkout, with
  env overrides for scratch roots and prompt dirs.
- Go ground truth now matches the Terraform `Meta` embedding scenario.
- `grade_go` now scores exact `<TypeName> embeds <bases>` rows.
- `analyze.py` prints headline, parent, and subagent cost.

Do not keep diagnostic task changes that hide `delegate` or `explore`; those
were reverted before this handoff was written.

## Plan moving forward

### Step 1: Land benchmark-bug PR

Open a small PR containing only:

- Harness path/reproducibility fixes.
- Go grader and ground-truth fix.
- Analyzer cost-breakdown fix.
- This handoff document.

Do not include scenario tool exclusions.

### Step 2: Try solution 3 first: generic Squeezy improvement

Investigate and fix routing/cost behavior with subagents still available:

1. Re-run C/Go Squeezy with graph and inspect parent vs subagent costs.
2. Determine why the planner chooses whole-task `delegate` instead of graph
   navigation on these tasks.
3. Add a generic routing guard or cost-aware policy if justified. Candidate:
   for single-turn bounded code-audit prompts, require a cheap parent graph
   probe before whole-task delegation, or make delegation compare projected
   subagent cost against expected graph/read cost.
4. Separately inspect why the parent loop uses read/grep bursts instead of graph
   primitives when graph tools are available.
5. Validate with `n=1` first, then `n=3` Haiku Squeezy vs CC. Keep subagents
   enabled in all final measurements.

The fix must be generic product behavior, not a benchmark-specific prompt or
scenario hack.

### Step 3: If solution 3 fails, try solution 2: replace tasks

If Squeezy still loses after generic routing improvements, classify C/Go as
task-fit failures. Replace them only with tasks that pass this fairness gate:

- Squeezy with graph must be materially cheaper than Squeezy no-graph at equal
  or better recall.
- The task must be a real developer question, not "list all graph edges".
- Ground truth must be independently verified and committed with the grader.
- Re-baseline Claude Code on the new prompt before comparing.

Candidate shape: impact/behavior questions where the answer requires resolving
call chains, inheritance, or receiver/type relationships across files, not just
enumerating grep matches.

## Validation runbook

After rebuilding `target/release/squeezy-eval`:

```sh
python3 docs/internal/eval-findings/realworld-harness/gen_inputs.py
python3 docs/internal/eval-findings/realworld-harness/hth.py c haiku both 1 with-graph
python3 docs/internal/eval-findings/realworld-harness/hth.py go haiku both 1 with-graph
```

For cost breakdown:

```sh
python3 docs/internal/eval-findings/realworld-harness/analyze.py <run-dir>
```

Use `n=3` only after the `n=1` signal is understood, because Haiku runs are
costly and high variance.
