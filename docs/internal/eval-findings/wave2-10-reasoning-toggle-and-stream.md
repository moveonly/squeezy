# wave2-10-reasoning-toggle-and-stream — eval finding report

- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-10-reasoning-toggle-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-10-reasoning-toggle-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-10-reasoning-toggle-portkey.toml`
- **Run dirs (relative to repo root):**
  - `target/eval/wave2-10-reasoning-toggle-openai-1780144015516/`
  - `target/eval/wave2-10-reasoning-toggle-anthropic-1780144100340/`
  - `target/eval/wave2-10-reasoning-toggle-portkey-1780144164834/`
- **Providers exercised:** OpenAI (`gpt-5.4-mini` @ `reasoning_effort = "low"`),
  Anthropic (`claude-haiku-4-5-20251001` @ `reasoning_effort = "high"`),
  Portkey (`@openrouter/qwen/qwen3.6-35b-a3b` @ `reasoning_effort = "low"`).
- **Auto-findings fired:** 0 in each run. The TUI-driven assertion
  failures (`asserted_fail: frame does not contain "▾ reasoning"`, etc.)
  surface as `action_step` rows in `trace.jsonl` but no rule in
  `crates/squeezy-eval/src/findings.rs` converts those rows to
  `findings.jsonl` entries.

## Summary

Three defects surfaced. Severity is `medium` for each by the wave-2
triage rules (cross-provider regression + provider-not-configured + UX
phrasing inconsistency affecting every reasoning turn). One is a real
TUI Ctrl+O regression that only one provider trips; one is a phrasing
inconsistency across the streaming → committed reasoning render path;
the third is an eval-harness defect that turns a provider config error
into an abort instead of a finding.

| # | Provider | Headline | Severity | Rubric dimension | File:line | bd |
|--:|---|---|---|---|---|---|
| 1 | OpenAI | gpt-5.4-mini @ low effort emits no reasoning chip; Ctrl+O silently collapses the assistant message | medium | functionality + cross-model consistency | `crates/squeezy-tui/src/lib.rs:3400`, `crates/squeezy-tui/src/lib.rs:3508` | `squeezy-a88` |
| 2 | Anthropic (and any provider that streams reasoning) | streaming header reads `▾ thinking…` then post-turn flips to `▸ reasoning:` — same content, three nouns, two chevrons | medium | visual clarity + messaging | `crates/squeezy-tui/src/lib.rs:5478`, `crates/squeezy-tui/src/lib.rs:6594` | `squeezy-lu9` |
| 3 | Portkey | scenario aborts on missing `PORTKEY_API_KEY` instead of landing a `medium` finding in the run | medium | harness functionality | `crates/squeezy-eval/src/driver.rs` (provider resolve path) | `squeezy-00f` |

## Provider config error encountered

Portkey: `provider is not configured: missing PORTKEY_API_KEY or
SQUEEZY_PORTKEY_KEY`. Per the wave-2 hard rule this is a `medium`
finding (filed as `squeezy-00f`), not an abort. The harness, however,
*did* abort — see finding 3 — leaving behind an empty run directory
(`target/eval/wave2-10-reasoning-toggle-portkey-1780144164834/`
contains zero-byte `trace.jsonl`, `frames.jsonl`, `frames_tui.jsonl`,
`replay.tui` and no `run.json` / `findings.jsonl` at all).

## Finding 1 — OpenAI Ctrl+O collapses the assistant message when no reasoning chip exists

**Severity:** medium — cross-provider regression. Anthropic and (when
keyed) Portkey emit a reasoning entry, the test passes, the user gets
the expand they pressed for. OpenAI gpt-5.4-mini at
`reasoning_effort = "low"` ships zero reasoning summary tokens, so the
transcript holds only `[user_msg, assistant_msg]`. Ctrl+O then targets
the assistant body and **collapses** it (the binding is named
`ExpandSelectedTranscriptEntry`).

### Expected vs. observed

- Expected: a press of Ctrl+O on a turn that emitted no reasoning chip
  reports `nothing expandable yet · /expand all also works` (the empty
  case in `toggle_selected_transcript_entry`,
  `crates/squeezy-tui/src/lib.rs:3404`), **or** explicitly tells the
  user "no reasoning summary for this turn".
- Observed: status flips to `collapsed transcript entry 2 · Ctrl+E
  expand all` and the assistant body folds. The user can't tell the
  fold was their own keypress vs. an automatic collapse on turn end.

### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-10-reasoning-toggle-openai.toml \
  --no-triage
```

### Evidence

`target/eval/wave2-10-reasoning-toggle-openai-1780144015516/trace.jsonl`:

- seq 1: `harness_prompt … drained · 2 transcript entries · status="ready"`.
  Two entries means no Reasoning entry. Both are `Message` (`role=user`
  + `role=assistant`).
- seq 3: `assert tui_frame_contains "▸ reasoning"` →
  `asserted_fail: frame does not contain "▸ reasoning" · entries:
  [message|col=false|"Think briefly…"] [message|col=false|"For a tiny
  ~100-entry keyed cache, a hash map is usually the better fit…"]`.
- seq 5: `send_key Ctrl+O · status="collapsed transcript entry 2 ·
  Ctrl+E expand all"`. Same trace event also re-dumps the second entry
  as `col=true` — Ctrl+O flipped `entry.collapsed` on the assistant
  message.
- seq 7: `assert tui_frame_contains "▾ reasoning"` →
  `asserted_fail`. The frame has no reasoning chevron at all because no
  Reasoning entry was ever created.
- seq 9: `assert tui_frame_contains "▏"` → `asserted_fail` (no body
  marker, since reasoning never materialised).
- seq 11: `assert tui_status_contains "expanded"` →
  `asserted_fail: status="collapsed transcript entry 2 · Ctrl+E expand
  all" does not contain "expanded"`. (Status contains `expand all`
  but not `expanded`, which is correct — Ctrl+O collapsed, didn't
  expand.)

### Suspected cause

`crates/squeezy-tui/src/lib.rs:3400` `toggle_selected_transcript_entry`
falls through `selected → latest_collapsed_transcript_entry →
latest_toggleable_transcript_entry`. With no Reasoning entry and the
assistant message defaulting to `collapsed = false`, `latest_collapsed`
returns `None` and `latest_toggleable` (`crates/squeezy-tui/src/lib.rs:3508`)
returns the assistant message (toggleable via the
`text_has_collapsible_content` check at
`crates/squeezy-tui/src/lib.rs:11361` — the two-line wrap of the OpenAI
answer at the 160-cell scenario width counts as collapsible).

Fix sketch: when `app.show_reasoning_usage` is true and the transcript
has no `TranscriptEntryKind::Reasoning(_)` entry, bias the no-target
path to set `status` to "no reasoning trace for this turn · enable
reasoning_effort" instead of falling through to the assistant message.
The fallback assistant-collapse is still useful when the user *isn't*
in reasoning mode (`show_reasoning_usage = false`).

### Beads ticket

`squeezy-a88` (P1).

## Finding 2 — streaming reasoning header is `▾ thinking…`, post-turn header is `▸ reasoning:` / `▾ reasoning`

**Severity:** medium — visual clarity + messaging. Reproduces on every
reasoning-emitting turn (Anthropic Haiku high, OpenAI Responses with
summary, Qwen3 think-blocks). The user sees the chevron flip itself
the moment the turn finishes even though they didn't touch anything.

### Expected vs. observed

- Expected: the same noun (`reasoning`) and chevron direction
  (`▾` while open, `▸` once collapsed) across streaming and committed
  renders. Difference is only the trailing state marker
  (`(streaming…)` vs. `(N lines)`).
- Observed: streaming header is `▾ thinking…`, committed-collapsed
  header is `▸ reasoning:`, committed-expanded header is
  `▾ reasoning (N lines)`. The chevron flips direction (`▾` → `▸`) at
  turn end, which reads as if the entry auto-collapsed itself.

### Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-10-reasoning-toggle-anthropic.toml \
  --no-triage
```

Watch the live narration during the streaming phase — header is
`▾ thinking…`. Then assert seq 3 passes against `▸ reasoning` because
the entry coalesced and re-rendered with a *different* noun and chevron.

### Evidence

`target/eval/wave2-10-reasoning-toggle-anthropic-1780144100340/trace.jsonl`:

- seq 3: `asserted_pass` on `tui_frame_contains "▸ reasoning"` — after
  turn completion the header is `▸ reasoning:` (collapsed).
- seq 5: `send_key Ctrl+O · status="expanded transcript entry 2 ·
  Ctrl+E expand all"`.
- seq 7,9: pass on `▾ reasoning` and `▏` body marker — confirms the
  finalized expanded header uses `▾ reasoning (N lines)`, not
  `▾ thinking…`.

Code:

- `crates/squeezy-tui/src/lib.rs:5478` `streaming_reasoning_lines` builds
  the header as `▾ thinking…`.
- `crates/squeezy-tui/src/lib.rs:6594` `reasoning_block_lines_with_extras`
  builds the post-turn header as `▸ reasoning: <summary>` (collapsed)
  or `▾ reasoning (N lines)` (expanded).

### Suspected cause

Two render paths grew independently. The streaming preview pre-dates
the committed-entry style guide and was never updated to share noun /
chevron with the finalized form.

Fix sketch: make `streaming_reasoning_lines` emit
`▾ reasoning (streaming…)` — same `▾` direction (the entry is "open"
while text arrives) and same noun (`reasoning`). The trailing
`(streaming…)` is enough to mark in-flight content.

### Beads ticket

`squeezy-lu9` (P1).

## Finding 3 — Portkey provider config error aborts the run instead of landing a finding

**Severity:** medium — wave-2 hard rule requires provider config errors
to land as `medium` findings in `findings.jsonl`, not aborts. The
fixture itself notes this in its description block.

### Expected vs. observed

- Expected: `target/eval/wave2-10-reasoning-toggle-portkey-…/run.json`
  exists, lists the provider as `portkey`, and `findings.jsonl` carries
  one entry with `rule_id = "provider_not_configured"` (or similar) at
  `severity = medium`. Exit code reflects `--fail-on findings`.
- Observed: harness exits early before writing `run.json` /
  `findings.jsonl`. The run directory is created but contains only
  zero-byte `trace.jsonl`, `frames.jsonl`, `frames_tui.jsonl`, and
  `replay.tui`. Downstream `squeezy-eval check` and `squeezy-eval diff`
  consumers see a half-initialized state.

### Reproducer

```sh
unset PORTKEY_API_KEY SQUEEZY_PORTKEY_KEY
cargo run -p squeezy-eval -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-10-reasoning-toggle-portkey.toml \
  --no-triage
```

(or run from any environment without the Portkey key configured in
`~/.squeezy/settings.toml`).

### Evidence

Stderr line emitted before exit:

> `squeezy-eval: provider: provider is not configured: missing
> PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add
> `[providers.<name>] api_key = "…"` to ~/.squeezy/settings.toml or
> the project-local settings.toml`

`target/eval/wave2-10-reasoning-toggle-portkey-1780144164834/`:

```
findings.jsonl       <absent>
run.json             <absent>
trace.jsonl          0 bytes
frames.jsonl         0 bytes
frames_tui.jsonl     0 bytes
replay.tui           0 bytes
```

### Suspected cause

The `ProviderNotConfigured` error bubbles up through `EvalError` and
the binary's `main` prints it + exits before the per-run finalizer
runs. The fixture comment already calls out the expected behaviour
(`description = "…When the key is missing the
provider-not-configured path lands as a medium-severity finding in
the run, not an abort"`), so the contract is documented but
unimplemented.

Fix sketch: catch `EvalError::ProviderNotConfigured` (or the matching
variant) inside the run pipeline, synthesize a
`Finding { rule_id: "provider_not_configured", severity: Medium,
… }`, append it to `findings.jsonl`, write a minimal `run.json`
(provider, model, totals all zero, the one finding), and exit
non-zero only when `--fail-on findings` is set.

### Beads ticket

`squeezy-00f` (P2 — harness defect, not a runtime regression in
squeezy itself).

## Palette compliance

Both `streaming_reasoning_lines` and `reasoning_block_lines_with_extras`
render only `Modifier::DIM | Modifier::ITALIC` — no foreground / no
background. They inherit the terminal default fg, which is `QUIET`
(`palette.rs:36 Color::DarkGray`) effectively. No luminance violation
on the reasoning surface in any of the three runs.

The selected-entry `marker = "> "` prefix uses the same DIM+ITALIC
style with no fg, so the selection cue piggybacks on `QUIET`. No
finding here.

## How to re-run all three

```sh
source ~/.env.sh   # OPENAI_API_KEY, ANTHROPIC_API_KEY

# Portkey requires either PORTKEY_API_KEY exported, or
# [providers.portkey] api_key = "…" in ~/.squeezy/settings.toml.
# Without it, finding 3 reproduces.

for prov in openai anthropic portkey; do
  cargo run -p squeezy-eval -- run \
    crates/squeezy-eval/fixtures/scenarios/wave2-10-reasoning-toggle-${prov}.toml \
    --no-triage
done
```

Expected outcomes:

- OpenAI run: trace 12 events, frames 0, findings 0 — but four
  `asserted_fail` rows in `trace.jsonl` matching finding 1.
- Anthropic run: trace 12 events, frames 0, findings 0 — all four TUI
  assertions pass. Finding 2 is observable only by eyeballing the
  streaming-phase header against the post-turn one (no per-event
  assertion captures the mid-stream noun).
- Portkey run: harness aborts unless a Portkey key is configured; the
  abort is finding 3.

## Why no auto-finding fired

No rule in `crates/squeezy-eval/src/findings.rs` converts
`action_step.status` rows that start with `asserted_fail:` into
`findings.jsonl` entries. Existing rules cover
`unsupported_slash_command:` and `denied_no_action`, but not the
TUI-driven `asserted_fail:` family. Adding one would let the three
OpenAI assertion failures surface in `findings.jsonl` (and any future
TUI scenario benefit too) — would be a one-rule change in `findings.rs`
keyed on `status.starts_with("asserted_fail:")`. Out of scope here.
