# wave2-13-tool-output-spillover

Domain 13 of the wave-2 bug-hunt: drive a tool call whose stdout exceeds
`DEFAULT_TOOL_SPILL_THRESHOLD_BYTES` (25 KiB), then evaluate the surface
the model and the TUI see for the spill outcome (envelope shape, in-card
preview palette, recovery affordance, retention prune safety).

- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-13-tool-output-spillover-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-13-tool-output-spillover-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-13-tool-output-spillover-portkey.toml`
- **Run dirs:**
  - openai: `target/eval/wave2-13-tool-output-spillover-openai-1780145036329/`
  - anthropic: `target/eval/wave2-13-tool-output-spillover-anthropic-1780145231996/`
  - portkey: `target/eval/wave2-13-tool-output-spillover-portkey-1780145381437/` (provider not configured, see finding 5 — directory contains empty `trace.jsonl` / `frames.jsonl`)
- **Auto-findings fired:**
  - openai: 2x `approval_unanswered`, 2x `denied_tool_call_ux`
  - anthropic: 1x `approval_unanswered`, 1x `denied_tool_call_ux`
  - portkey: n/a

## Headlines

| # | Severity | Provider(s) | Rubric dimension | Headline | bd id |
|--:|---|---|---|---|---|
| 1 | major (P1) | openai | Messaging, Functionality | Spill envelope omits `read_tool_output` recovery hint and on-disk path | `squeezy-uq1g` |
| 2 | major (P1) | all | Visual clarity | TUI tool summary spans use `Color::White` (luminance 255) — palette guardrail violation | `squeezy-h7h6` |
| 3 | medium (P2) | openai | Progressive disclosure, Functionality | Spilled shell card drops original command from tool summary | `squeezy-9jab` |
| 4 | medium (P2) | openai | Functionality, Cross-model consistency | Eval driver `permission_mode` overlay does not extend to `permissions.read` — spill recovery auto-denied | `squeezy-03oa` |
| 5 | medium (P2) | portkey | Cross-model consistency | Portkey scenario cannot run — `PORTKEY_API_KEY` missing | `squeezy-17rv` |
| 6 | medium (P2) | anthropic | Cross-model consistency | Anthropic Haiku reshapes literal grep into pipeline, sidestepping spill probe | `squeezy-2c9c` |

## Reproduction

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-13-tool-output-spillover-openai.toml --no-triage
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-13-tool-output-spillover-anthropic.toml --no-triage
# portkey requires PORTKEY_API_KEY in env or [providers.portkey].api_key in ~/.squeezy/settings.toml
```

## Finding 1 — spill envelope omits recovery hint

**Severity:** major (P1) · **Rubric:** Messaging, Functionality · **Provider:** openai (reproducible) · **bd:** `squeezy-uq1g`

The `ToolOutputStore::maybe_spill` write at
`crates/squeezy-tools/src/lib.rs:4185-4200` is:

```rust
json!({
    "spilled": true,
    "handle": sha256,
    "sha256": sha256,
    "original_output_sha256": original_output_sha256,
    "total_bytes": output.len(),
    "preview_bytes": preview.len(),
    "preview": preview,
    "truncated": true,
})
```

There is no `recovery_tool` field and no on-disk path. The model has to
infer that `read_tool_output(handle=...)` exists from its registry, and
the user reading the TUI card has no hint at how to expand the spilled
result.

Evidence: `target/eval/wave2-13-tool-output-spillover-openai-1780145036329/trace.jsonl` seq 14 (spill envelope, 123007 bytes spilled to handle `0d99df7a...`), seq 100 (model spontaneously inferred `read_tool_output` — good for OpenAI but not guaranteed for less capable models), seq 101/103 (driver auto-denied the recovery — finding 4 explains why).

Fix sketch: append `recovery_tool: "read_tool_output"`, `recovery_args: { handle: <sha> }`, and `on_disk_path: "<workspace>/.squeezy/tool_outputs/<sha>.json"` to the spill JSON. Mirror the same text in the TUI card so a user can also `tail -f` the file if they prefer.

## Finding 2 — Color::White palette violation

**Severity:** major (P1) · **Rubric:** Visual clarity · **Provider:** all · **bd:** `squeezy-h7h6`

Per the wave-2 palette guardrail in
`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`, any cell with luminance
`0.299R + 0.587G + 0.114B > 160` is a finding. `Color::White = (255,
255, 255)` lands at luminance 255 and appears 33 times in
`crates/squeezy-tui/src/lib.rs`. Selected hot spots in the tool-summary
path that a spilled card flows through:

- `crates/squeezy-tui/src/lib.rs:7282` (default tool-row label)
- `crates/squeezy-tui/src/lib.rs:7319` (generic fallback summary)
- `crates/squeezy-tui/src/lib.rs:7422` (decl_search summary label)
- `crates/squeezy-tui/src/lib.rs:7443` (semantic_tool summary label)
- `crates/squeezy-tui/src/lib.rs:7462` (repo_map summary label)
- `crates/squeezy-tui/src/lib.rs:7492` (grep summary label)
- `crates/squeezy-tui/src/lib.rs:7525` (glob summary label)
- `crates/squeezy-tui/src/lib.rs:7542` (read_file summary label)
- `crates/squeezy-tui/src/lib.rs:7563` (read_tool_output "expand saved tool output" label)
- `crates/squeezy-tui/src/lib.rs:7582, 7695, 7732, 7768, 7865, 8143, 8682, 8699` (other summary/expanded paths)

The palette module already exposes a tone-aware `muted_fg()` blend
(`crates/squeezy-tui/src/render/palette.rs:370-383`) that lands well
under the luminance ceiling and is the natural replacement.

Evidence: `target/eval/wave2-13-tool-output-spillover-openai-1780145036329/frames.jsonl` line 1 — `styled_lines` shows summary spans rendered without an explicit `fg` (defaulting to terminal-white) for the tool labels.

## Finding 3 — spilled shell card loses the original command

**Severity:** medium (P2) · **Rubric:** Progressive disclosure, Functionality · **Provider:** openai · **bd:** `squeezy-9jab`

`ToolOutputStore::maybe_spill` at
`crates/squeezy-tools/src/lib.rs:4144` replaces the entire
`result.content` with the spill envelope. The original `command` field
is dropped (it shows up only inside the JSON-encoded `preview` blob,
which the summary spans do not parse).

`shell_tool_summary_spans` at `crates/squeezy-tui/src/lib.rs:7372`
reads `command` from `tool.call.arguments['command']` or
`tool.result.content['command']`, falling back to the literal `"shell"`.
On any code path that reconstructs the `ToolTranscript` from the result
alone (resumed session, replay, eval re-render), the spilled card
displays just `shell · more available` with no hint at what ran.

Evidence: openai run trace seq 14 — the spill `content` has no `command` field.

Fix sketch: in `maybe_spill`, preserve the original `command` (and a
short `workdir`) alongside the spill envelope, or copy them from the
captured `ToolCall` so summary spans render the command without
depending on the call arguments being threaded through.

## Finding 4 — eval driver `permission_mode` overlay leaves `read` untouched

**Severity:** medium (P2) · **Rubric:** Functionality, Cross-model consistency · **Provider:** openai surfaced it · **bd:** `squeezy-03oa`

`crates/squeezy-eval/src/driver.rs:476-485`:

```rust
config.permissions.edit = mode;
config.permissions.shell = mode;
config.permissions.web = mode;
config.permissions.mcp = mode;
```

`config.permissions.read` (and `ignored_search`) are NOT touched. When
the dispatching machine's `~/.squeezy/settings.toml` carries
`[permissions] read = "ask"` — a perfectly reasonable developer
default — any Read-scope tool surfaces an `ApprovalRequested`, and the
eval harness driver auto-denies it (rule `approval_unanswered`).

`read_tool_output` is a Read capability
(`crates/squeezy-tools/src/lib.rs:1899-1912`), so the spill-recovery
loop the scenario was supposed to exercise never runs end-to-end on a
machine with `read = "ask"`.

Evidence: openai run findings.jsonl shows 2× `approval_unanswered`
(planner `definition_search` + model's `read_tool_output` recovery).
The model DID infer `read_tool_output` (good), but the recovery was
blocked at the gate.

Fix sketch: in `apply_overlay`, extend assignment to cover Read scopes:

```rust
config.permissions.read = mode;
config.permissions.ignored_search = mode;
```

Document the broadening in `docs/internal/EVAL_HARNESS.md` so scenario
authors know `permission_mode = "allow"` is comprehensive.

## Finding 5 — portkey provider not configured

**Severity:** medium (P2) · **Rubric:** Cross-model consistency · **Provider:** portkey · **bd:** `squeezy-17rv`

Per the wave-2 brief, providers in rotation include
`portkey` (Qwen3 via OpenRouter) and the dispatching env must already
have the key configured. On this machine neither `PORTKEY_API_KEY` nor
`SQUEEZY_PORTKEY_KEY` is exported, and `~/.squeezy/settings.toml` does
not contain a `[providers.portkey]` section.

Result: `wave2-13-tool-output-spillover-portkey.toml` aborted at
provider resolution with

```
provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY;
set the env var or add [providers.<name>] api_key = "…" to ~/.squeezy/settings.toml
```

Per the wave-2 rule ("provider config error → medium finding, not
abort"), this is filed as medium; the other two providers ran. We
cannot answer the cross-model question of whether Qwen's chatty-
preamble pattern interferes with the spill-recovery follow-up call
without a successful Portkey run.

## Finding 6 — Anthropic Haiku reshapes literal grep into pipeline

**Severity:** medium (P2) · **Rubric:** Cross-model consistency · **Provider:** anthropic · **bd:** `squeezy-2c9c`

The scenario asks the model to run `grep -rE 'fn ' crates/
--include='*.rs'` literally and exactly once. OpenAI gpt-5.4-mini did
(spill envelope fired). Anthropic Haiku silently rewrote the command:

```
grep -rE 'fn ' crates/ --include='*.rs' | wc -l && \
grep -rE 'fn ' crates/ --include='*.rs' | cut -d: -f1 | sort | uniq -c | sort -rn | head -20
```

Shaped stdout is 2166 bytes, well under the 25 KiB threshold, so the
spill envelope never fires on Anthropic. The model is being "helpful"
but the side-effect is that domain-13 cannot be answered against Haiku
at all from this scenario shape.

Evidence:
- `target/eval/wave2-13-tool-output-spillover-anthropic-1780145231996/trace.jsonl` seq 13 (shaped command).
- `target/eval/wave2-13-tool-output-spillover-openai-1780145036329/trace.jsonl` seq 13 (literal command).
- Compare the two frames' `tool_calls[0].args_preview` directly.

Fix sketch (scenario, not product, unless a product-level pass-through
hint is added to the shell tool spec): tighten the Haiku prompt with an
explicit imperative — "Do not modify the command; the literal grep must
run unaltered as a single shell call" — or set
`tool_choice = "required"` plus `instructions = "Pass through the exact
command supplied by the user."` If the divergence persists, the
product-side fix is to add a literal-command preserving directive to
the shell tool spec.

## Cross-cutting observation

The Functionality dimension of domain 13 also asked whether the
retention prune deletes the just-spilled file. The prune
(`ToolOutputStore::cleanup_old_outputs` at
`crates/squeezy-tools/src/lib.rs:4227`) runs **only at construction
time** in `ToolOutputStore::new` (line 4140), not at every spill, so a
freshly written `<workspace>/.squeezy/tool_outputs/<sha>.json` is safe
within the same process. Direct end-to-end verification was not
possible from these runs because the snapshot workspace was cleaned up
by the eval `Drop` guard after each run, and finding 4 prevented the
in-run `read_tool_output` from completing. No finding filed on the
prune path; the code reads correctly.
