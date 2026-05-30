# wave2-03 slash-help-discovery — eval finding report

- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-03-slash-help-discovery-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-03-slash-help-discovery-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-03-slash-help-discovery-portkey.toml`
- **Run directories:**
  - OpenAI: `target/eval/wave2-03-slash-help-discovery-openai-1780145629088`
  - Anthropic: `target/eval/wave2-03-slash-help-discovery-anthropic-1780145728805`
  - Portkey: `target/eval/wave2-03-slash-help-discovery-portkey-1780145802944` (empty — provider misconfigured)
- **Status:** 1 medium finding (provider config error). Rendering /
  fuzzy-match / progressive-disclosure probes pass on OpenAI and
  Anthropic.

## Probe shape

Each scenario drives the same four-stage script through the
`TuiHarness` (drive_tui = true, 160×48, palette_tone = "dark"):

1. `/help` (curated topic index, system transcript message).
2. `/help providers` (curated topic answer with `docs/external/PROVIDERS.md`
   citation and an extracted `[model]` config-inspect section).
3. Slash-menu fuzzy ranking: type `/com` (expect `/compact` row + GOLD
   `›` selected marker), `Ctrl+U` to clear, then `/atc` (expect the
   non-prefix-but-subsequence hit `/attach` per
   `input_tests.rs:slash_suggestions_match_substring_not_just_prefix`).
4. `/help frobulator-widgets` — unsupported topic. The
   `SqueezyHelp::unsupported` body lists every bundled topic id so the
   user has a recovery surface (`crates/squeezy-skills/src/help.rs:159`).

All assertions key on `tui_frame_contains` rather than `text_contains`
because curated `/help` bodies are pushed to the TUI transcript as
system messages, not as assistant stream text — `last_assistant_text`
stays empty across the whole run.

# Finding 1: PORTKEY_API_KEY missing — slash-help-discovery portkey probe never runs

## Severity

medium — provider config error (per
`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md` hard rule
"Provider config error → medium finding"). The Portkey leg of the
wave-2 / 03 row cannot execute, so the **Cross-model consistency**
rubric dimension (rubric #6) is unverifiable for this domain.

## What you should see vs. what you see

- Expected: `cargo run -p squeezy-eval ... wave2-03-slash-help-discovery-portkey.toml --no-triage` runs the same 4-stage probe the openai / anthropic legs run.
- Observed: scenario aborts before step 1 with
  `provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY; set the env var or add [providers.<name>] api_key = "..." to ~/.squeezy/settings.toml or the project-local settings.toml`. Empty `trace.jsonl`; the wave-2 plan's provider-key prerequisite is not satisfied on the dispatching account.

## Reproducer

```sh
source ~/.env.sh
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-03-slash-help-discovery-portkey.toml \
  --no-triage
```

Observed exit at start: `squeezy-eval: provider: provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY` followed by the harness hint pointing at `docs/internal/EVAL_HARNESS.md`.

## Evidence

- Run dir `target/eval/wave2-03-slash-help-discovery-portkey-1780145802944/trace.jsonl` is **empty** — no `action_step` events were recorded; the provider preflight in `crates/squeezy-eval/src/driver.rs:140` failed before the first step boundary.
- The dispatching env has no `PORTKEY_API_KEY` / `SQUEEZY_PORTKEY_KEY` exported, and `~/.squeezy/settings.toml` carries no `[providers.portkey] api_key = "…"` entry (verified by the absence of any `portkey` provider in the run's resolved `AppConfig`; not by reading the settings file, per the wave-2 dispatcher's no-secrets-probe guardrail).
- Plan expectation, `docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md:18-19`: "`portkey` — model `@openrouter/qwen/qwen3.6-35b-a3b`, requires `[providers.portkey].api_key` in `~/.squeezy/settings.toml`."

## Suspected cause

Environment / configuration only. No code defect: the squeezy-eval preflight short-circuits with the exact remediation hint a real operator would need. The wave-2 plan requires three live provider scenarios per domain; this third leg cannot be exercised until the dispatching account exports the Portkey key.

## Tracking

- `squeezy-hg94` — `bd create --type bug --priority 1 ...`
  Title: `wave2-03: PORTKEY_API_KEY missing — slash-help-discovery portkey probe never runs`
  Status: open.

## Other rubric dimensions — observations

Both runnable scenarios (OpenAI gpt-5.4-mini, Anthropic
claude-haiku-4-5) pass every assertion. The probes the harness can
make against these surfaces did not surface a defect:

- **Functionality (rubric 2).** `/help` and `/help providers` both
  render through the curated `SqueezyHelp` short-circuit. The
  rendered frame contains `Supported topics`, every required topic
  head (`providers`, `permissions`), and for `/help providers` it
  carries both the `PROVIDERS.md` citation and the `[model]`
  config-inspect section. The agent never escalates to the model for
  these — cost is `$0.0000` on the OpenAI leg as expected.
- **Functionality / progressive disclosure (rubric 5).** Slash menu
  appears on `/com` and ranks `/compact` highly enough that
  `tui_frame_contains "/compact"` passes immediately after the four
  keystrokes land. The selected-row chevron `›` paints on the same
  frame. Fuzzy hit `/atc → /attach` also resolves after `Ctrl+U`
  clears the composer.
- **Messaging (rubric 3).** `/help frobulator-widgets` lands in the
  curated `unsupported` body, which lists every bundled topic id so
  the user has a recovery surface. The closing frame contains
  `providers` as part of "Try one of these local topics: agent,
  config, providers, ...".
- **Cross-model consistency (rubric 6).** OpenAI and Anthropic
  produce identical curated frames (the help skill is
  provider-independent — see `crates/squeezy-agent/src/lib.rs:3410`
  `resolve_help_turn` — so any divergence between them would itself
  be a finding). Cannot be verified against Portkey until the key
  configuration is fixed; that gap is captured as the medium finding
  above.

## Harness ergonomic notes (no ticket — log only)

The harness's `text_contains` assertion is keyed on
`Driver::last_assistant_text` (`crates/squeezy-eval/src/driver.rs:925`),
which is only populated by streamed assistant tokens. Curated `/help`
short-circuits push the body as a system transcript item via
`TranscriptItem::system` (`crates/squeezy-tui/src/lib.rs` help dispatch
at `2851`); the assistant_text channel never carries it. For wave-2
probes against locally-handled slash commands, prefer
`tui_frame_contains` or `tui_transcript_entry`.

Esc on a quiescent composer is a no-op (`lib.rs:1413`) — clearing the
composer mid-scenario requires `Ctrl+U` (`lib.rs:1332`,
`delete_to_line_start`) or repeated `Backspace`. Worth surfacing in
`EVAL_HARNESS.md` "Recipes" so the next wave-2 author doesn't pay the
same tax.

## How to re-run

```sh
source ~/.env.sh

# OpenAI leg
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-03-slash-help-discovery-openai.toml \
  --no-triage

# Anthropic leg
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-03-slash-help-discovery-anthropic.toml \
  --no-triage

# Portkey leg (currently fails preflight per finding above)
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-03-slash-help-discovery-portkey.toml \
  --no-triage
```

Expected: trace 32 events / frames 0 / findings 0 / cost $0.0000 on the
OpenAI and Anthropic legs (curated help short-circuits do not bill).
The Portkey leg currently exits before recording any trace; resolution
is documented under the bd ticket above.
