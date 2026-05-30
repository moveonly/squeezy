# wave2-15 working-card-and-spinner — eval finding report

- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-15-working-card-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-15-working-card-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-15-working-card-portkey.toml`
- **Run dirs:**
  - openai: `target/eval/wave2-15-working-card-openai-1780146527316/`
  - anthropic: `target/eval/wave2-15-working-card-anthropic-1780146581300/`
  - portkey: `target/eval/wave2-15-working-card-portkey-1780146636033/` (provider config error — see Finding 3)
- **Auto-findings fired:** 0 across the two live runs; the harness has
  no rule that audits palette luminance, so the visual-clarity findings
  below are evidence-cited code-path analyses.

## Probe

Working-card detail rows (current tool name, per-tool elapsed,
queued-next preview) + shimmer animation. Cross-check the AMBER brand
colour reaches the balls/spinner. Spinner must pause when
`app.focused = false`. Elapsed time advances on tick.

The probe drives a tiny live turn through `TuiHarness` so the post-turn
frame paints (the harness's 180s `pump_until_idle` deadline excludes
multi-tool grep+read prompts, which is the next finding below). Each
scenario asserts on the `●` brand coin glyph (rendered by
`prompt_coin_span` at `crates/squeezy-tui/src/lib.rs:9147`) and the
`Worked for` divider (rendered by `worked_divider_line` at
`crates/squeezy-tui/src/lib.rs:5023`). Both asserts pass on the
openai and anthropic runs.

The visual findings come from inspecting `working_line`,
`working_word_spans`, `shimmer_word_spans`, and `active_tool_spans`,
plus `render::palette::accent_primary` /
`render::palette::accent_working_highlight`. The brand-colour pieces
do flow from AMBER (`Rgb(215, 147, 52)`); however, two cells in the
working-card render path render at luminance > 160 in violation of
the wave-2 palette guardrail (`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`).

## Probe verification (functional path)

| Surface | Source | Brand colour resolved | Outcome |
|---|---|---|---|
| Working bullet `• ` | `working_line:4937` → `accent_primary()` | AMBER `Rgb(215, 147, 52)` | OK (default accent) |
| Spinner / `Working` shimmer base | `shimmer_word_spans:5005` → `accent_primary()` | AMBER | OK (default accent) |
| Prompt coin `●` (idle) | `prompt_coin_span:9152` → `AMBER` constant | AMBER | OK |
| Per-tool elapsed `· Ns` | `active_tool_elapsed_spans:7874` → `QUIET` | DarkGray | OK |
| Queued-tools detail row | `working_detail_line:4877` → `QUIET` | DarkGray | OK |
| Tool name in active row | `active_tool_spans:7847` → `AMBER` bold | AMBER | OK |
| Elapsed clock value `(<N>s · esc to interrupt)` | `current_turn_duration:5016` → `Instant::elapsed()` | n/a (advances by wall clock) | OK; display refresh paused by focus gate |
| Focus-loss spinner pause | main loop `if app.focused` gate at `lib.rs:587` + `has_active_animation()` | n/a | OK (regression test at `lib_tests.rs:9197`) |

Functionally the working card surfaces the right cues. The
defects below are visual (luminance) and tooling (Portkey config).

## Finding 1 — `active_tool_spans` args text is hardcoded `Color::White`

**Severity:** medium — palette guardrail violation, applies whenever a
non-shell tool is active in the working card (i.e. the most common
mid-turn working-card surface for `read_file`, `grep`, `decl_search`,
`apply_patch`, etc.).

**Rubric dimension:** Visual clarity (palette guardrail).

**File:line:** `crates/squeezy-tui/src/lib.rs:7865`.

**Ticket:** `squeezy-x6e7`.

### Evidence

`active_tool_spans` builds the per-tool segment appended to the
working line in `working_line` (lib.rs:4969). The arguments span for
any tool other than `shell` / `verify` is forced to `Color::White`:

```rust
spans.push(Span::styled(
    compact_text(&args, 80),
    Style::default().fg(Color::White),    // crates/squeezy-tui/src/lib.rs:7865
));
```

`Color::White` resolves to `Rgb(255, 255, 255)` (luminance 255), which
violates the dark-only rule documented in
`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`:

> Rule of thumb: any RGB whose luminance `0.299*R + 0.587*G + 0.114*B`
> exceeds ~160 is too bright and is a finding regardless of where it
> appears in the UI.

The same row also already uses `palette::muted_fg()` and
`palette::QUIET` for adjacent spans, so the per-tool arguments line is
the only bright span in the working card — it visually drowns out
the AMBER tool name, breaking the "amber identity > white preview"
hierarchy the rest of the palette enforces.

### Suspected fix

Replace `Color::White` with `palette::muted_fg()` (tone-aware grey,
already used by the body lines of completed tool cards) so dark mode
sees a sub-160-luminance preview. Alternatively, a new
`palette::accent_args_fg()` constant that resolves to a calmer
warm-grey (`~Rgb(170, 160, 150)`, luminance ~160) keeps the args
visually subordinate to the AMBER tool name while staying inside the
palette rule.

## Finding 2 — `WORKING_SHIMMER_HIGHLIGHT` blows luminance 250

**Severity:** medium — palette guardrail violation, intrinsic to the
working-card shimmer animation that fires every turn.

**Rubric dimension:** Visual clarity (palette guardrail).

**File:line:** `crates/squeezy-tui/src/render/palette.rs:38` (constant)
+ `crates/squeezy-tui/src/lib.rs:5005-5009` (blend site).

**Ticket:** `squeezy-txko`.

### Evidence

`shimmer_word_spans` blends `accent_primary()` (AMBER) toward
`accent_working_highlight()` per character based on the wave
intensity. The default-accent highlight is
`WORKING_SHIMMER_HIGHLIGHT = Rgb(255, 251, 235)`:

```rust
// crates/squeezy-tui/src/render/palette.rs:38
pub(crate) const WORKING_SHIMMER_HIGHLIGHT: Color = Color::Rgb(255, 251, 235);
```

Luminance:

```
0.299 * 255 + 0.587 * 251 + 0.114 * 235
= 76.245 + 147.337 + 26.79
= 250.37
```

That is ~50% above the palette-rule ceiling of 160, and it lands on
the central glyphs of `Working` at every shimmer crest — the most
eye-catching cells of the working card. The blend formula at
`crates/squeezy-tui/src/lib.rs:5004-5010` reaches the highlight at
peak (`intensity = 1.0`) on `(period - 1) / 2` ticks of every 3.4-second
sweep, so the bright pulse is sustained, not transient.

### Suspected fix

Pick a sub-160-luminance highlight that still pops against AMBER. A
candidate that keeps the warm hue:

```rust
pub(crate) const WORKING_SHIMMER_HIGHLIGHT: Color = Color::Rgb(231, 178, 90);
// luminance = 0.299*231 + 0.587*178 + 0.114*90 ≈ 184 → still too bright
```

A tighter option (`Rgb(212, 158, 80)`, luminance ≈ 161 → at the cap)
or capping the blend at `intensity * 0.6` keeps the sweep visible
without breaching the rule.

## Finding 3 — Portkey provider config missing on the dispatcher

**Severity:** medium — per the wave-2 brief
(`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`), provider configuration
errors are a `medium` finding rather than an abort.

**Rubric dimension:** Cross-model consistency (the Qwen leg cannot be
evaluated until the provider key is wired).

**File:line:** error originates from
`AppConfig::from_env_and_settings_with_provider` (in
`crates/squeezy-core`) once the eval driver re-resolves config against
the `portkey` preset (`crates/squeezy-eval/src/driver.rs:472`+ area).

**Ticket:** `squeezy-j8yi`.

### Evidence

```
squeezy-eval: provider: provider is not configured: missing
PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add
`[providers.<name>] api_key = "…"` to ~/.squeezy/settings.toml or
the project-local settings.toml
```

The wave-2 brief specifies the Portkey key lives in
`~/.squeezy/settings.toml`. The brief's "Hard rules" forbid this agent
from reading settings/env API-key files directly, so we cannot verify
which side is miswired (env vs. settings). The error is recorded and
the medium finding stands.

Run dir of the failing attempt:
`target/eval/wave2-15-working-card-portkey-1780146636033/` — contains
empty `frames_tui.jsonl`, `frames.jsonl`, `replay.tui`, and a
`trace.jsonl` that never reached `start_user_turn` (the driver bailed
before constructing the harness).

### Suspected fix

Either export `PORTKEY_API_KEY` (or `SQUEEZY_PORTKEY_KEY`) in
`~/.env.sh` so the dispatcher's `source ~/.env.sh` pulls it in, or
verify the `[providers.portkey] api_key = "…"` entry actually sits in
`~/.squeezy/settings.toml` and is readable by the eval process. Once
wired, re-run
`cargo run -p squeezy-eval --quiet -- run
crates/squeezy-eval/fixtures/scenarios/wave2-15-working-card-portkey.toml
--no-triage`. The fixture is otherwise unchanged.

## Probe limitations (not a defect — recorded for next-wave)

The current `TuiHarness::pump_until_idle` has a hard 180s deadline
(`crates/squeezy-tui/src/testing.rs:102`). Multi-tool prompts that
exercise the working-card mid-turn (`grep` + `read_file` sequence)
routinely exceed that budget against the OpenAI gpt-5.4-mini model
when the workspace is a fresh snapshot of squeezy itself, because the
agent's first turn does a large RepoProfile + system-prompt
serialization plus several tool round-trips. The probe therefore
falls back to a one-shot "Reply with the single word ok." prompt so
the harness drains; the working-card visual analysis is then carried
by code-path inspection (file:line evidence above) rather than a
mid-turn frame snapshot. A future TuiHarness extension that captures
a frame mid-stream (e.g. on `tool_call_started`) would let the
scenario assert `tui_frame_contains "Working"` and `"esc to
interrupt"` against the live spinner instead.

## How to re-run

```sh
source ~/.env.sh
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-15-working-card-openai.toml \
  --no-triage
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-15-working-card-anthropic.toml \
  --no-triage
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-15-working-card-portkey.toml \
  --no-triage    # currently errors with provider config (Finding 3)
```

Expected for the live runs: 8 trace events, both asserts (`●` and
`Worked for`) pass, 0 auto-findings, ~$0.00 cost (the prompt is one
word so the model bill is negligible).

## Severity summary

| # | Finding | Severity | Dimension | Ticket | File:line |
|--:|---|---|---|---|---|
| 1 | `active_tool_spans` args white | medium | Visual clarity | squeezy-x6e7 | `crates/squeezy-tui/src/lib.rs:7865` |
| 2 | shimmer highlight luminance 250 | medium | Visual clarity | squeezy-txko | `crates/squeezy-tui/src/render/palette.rs:38` |
| 3 | Portkey provider not configured | medium | Cross-model consistency | squeezy-j8yi | (config) |
