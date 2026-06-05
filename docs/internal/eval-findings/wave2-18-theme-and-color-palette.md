# wave2-18 theme-and-color-palette — eval findings

Status: historical snapshot. Current `drive_tui` slash-command dispatch routes
through the live TUI harness, and `TuiHarness` now receives the resolved
provider label. Keep the provider-label and TUI-only dispatch findings below as
captured run evidence, not current harness behavior.

## Area

Wave-2 row 18 (`theme-and-color-palette`) from
`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`. Probes the live `/theme`
slash command and audits every surface the harness reaches against the
dark-only palette guardrail
(`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md` "Palette guardrails":
luminance `0.299*R + 0.587*G + 0.114*B` must be ≤ 160 for any
rendered foreground cell).

## Scenarios

- `crates/squeezy-eval/fixtures/scenarios/wave2-18-theme-palette-openai.toml`
  — `provider = "openai"`, `model = "gpt-5.4-mini"`.
- `crates/squeezy-eval/fixtures/scenarios/wave2-18-theme-palette-anthropic.toml`
  — `provider = "anthropic"`, `model = "claude-haiku-4-5-20251001"`,
  `max_output_tokens = 2048`.
- `crates/squeezy-eval/fixtures/scenarios/wave2-18-theme-palette-portkey.toml`
  — `provider = "portkey"`,
  `model = "@openrouter/qwen/qwen3.6-35b-a3b"`,
  `tool_choice = "required"`.

All three boot the TUI, send a near-zero-cost prompt, then drive
`/theme dark` → `/theme catppuccin` → `/theme dark` through the
`TuiHarness` composer (`send_keys` typing the literal slash command +
Enter so `handle_slash_command → apply_theme_change →
apply_theme_overrides` actually runs — the eval slash-dispatch path
returns `DispatchOutcome::TuiOnly { command: "theme" }` and does not
flip the runtime palette on its own).

## Run artifacts

- OpenAI live run: `target/eval/wave2-18-theme-palette-openai-1780145769214/`
  - `trace.jsonl`: 16 events, all 4 asserts `asserted_pass`.
  - `findings.jsonl`: empty.
  - `run.json`: 0 auto-findings, 0 cost (below display threshold).
- Anthropic live run:
  `target/eval/wave2-18-theme-palette-anthropic-1780145816399/`
  - `trace.jsonl`: 16 events, all 4 asserts `asserted_pass`.
  - `findings.jsonl`: empty.
- Portkey live run: **did not execute**. Provider config error:
  `provider is not configured: missing PORTKEY_API_KEY or
  SQUEEZY_PORTKEY_KEY; set the env var or add
  [providers.<name>] api_key = "…" to ~/.squeezy/settings.toml`.
  Already filed as `squeezy-5ce` for the slash-help-discovery
  domain; the same precondition blocks every wave-2 portkey leg. Per
  the hard rules this is a medium finding, not an abort — recorded
  here for cross-reference; no new ticket.

## Findings

### F1 — eval harness `/theme` persists to the user's real `~/.squeezy/settings.toml` (NEW; HIGH/major)

- **Ticket:** `squeezy-ramu`.
- **Rubric:** Functionality + Cross-model consistency.
- **File:line:** `crates/squeezy-tui/src/lib.rs:1888`
  (`apply_theme_change`) — calls
  `squeezy_core::default_settings_path()`
  (`crates/squeezy-core/src/lib.rs:6894`) which prefers
  `$SQUEEZY_SETTINGS_PATH`, then `$HOME/.squeezy/settings.toml`.
  `crates/squeezy-tui/src/testing.rs:39-69` (`TuiHarness::new`)
  never points `SQUEEZY_SETTINGS_PATH` at a scratch file.
- **Evidence:**
  `target/eval/wave2-18-theme-palette-openai-1780145642914/trace.jsonl`
  seq 7 records `status: "sent 12 keys · status=\"theme saved to
  /Users/abbassabra/.squeezy/settings.toml\""`. After the run, the
  operator's real `~/.squeezy/settings.toml` contained
  `[tui] theme = "dark"` written by the harness.
- **Why it matters:** every wave-2 scenario that drives `/theme`,
  `/effort`, `/verbosity`, `/statusline`, `/permissions` etc. now
  clobbers the operator's real config. Scenario isolation is
  retroactively broken.
- **Provider:** all three (cross-cutting harness defect).

### F2 — Historical: `TuiHarness::new` hardcoded provider label as `"eval-harness"` (medium)

- **Ticket:** `squeezy-16k6`.
- **Rubric:** Cross-model consistency + Functionality.
- **File:line:** `crates/squeezy-tui/src/testing.rs:52-53` passes the
  literal `"eval-harness"` as the first argument to
  `TuiApp::new_with_clipboard`, used as the provider label in the
  banner card and status line.
- **Evidence:**
  `target/eval/wave2-18-theme-palette-anthropic-1780145672192/trace.jsonl`
  seq 5 preview shows `model: eval-harness:claude-haiku-4-5-20251001`
  instead of `anthropic:…`. The wave2-01 startup-banner scenarios
  assert on `<provider>:<model>` strings that this harness defect
  would also defeat.
- **Workaround in this domain:** assertions were narrowed from
  `<provider>:<model>` to `:<model>` so the rest of the probe runs.
- **Provider:** all three.

### F3 — `WORKING_SHIMMER_HIGHLIGHT` luminance 250 violates dark-only cap (NEW; medium)

- **Ticket:** `squeezy-8r6w`.
- **Rubric:** Visual clarity.
- **File:line:** `crates/squeezy-tui/src/render/palette.rs:38`
  (`WORKING_SHIMMER_HIGHLIGHT = Rgb(255, 251, 235)`, luminance 250.4)
  and `:191-197` (`accent_working_highlight` per-variant overrides).
  Blend call site:
  `crates/squeezy-tui/src/lib.rs:5004-5008` interpolates from
  `accent_primary()` (AMBER lum 156.5) to the highlight at cosine
  intensity 0..1.
- **At peak intensity:** the cell is rendered as the highlight value
  verbatim. Default → 250.4; Catppuccin (`Rgb(245, 224, 220)`) →
  229.8; HighContrast (`Rgb(255, 255, 255)`) → 255. All over the 160
  cap.
- **Why it matters:** the working-card shimmer is one of the most
  recognisable "this is squeezy talking" cues; at peak it briefly
  becomes the brightest cell on screen, drowning the AMBER brand it
  shimmers against.
- **Provider:** all three (rendering is identical).

### F4 — `AccentVariant::Catppuccin` mauve and `HighContrast` yellow exceed luminance cap (NEW; medium)

- **Ticket:** `squeezy-ybo8`.
- **Rubric:** Visual clarity + Cross-model consistency.
- **File:line:** `crates/squeezy-tui/src/render/palette.rs:181-187`
  (`accent_primary()` match arms).
- **Calculations:**
  - `Catppuccin` → `Rgb(203, 166, 247)` → luminance 186.3.
  - `HighContrast` → `Rgb(255, 255, 0)` → luminance 225.9.
  - `Default` (AMBER `Rgb(215, 147, 52)`) → luminance 156.5 (the only
    compliant accent).
- **Evidence:**
  `target/eval/wave2-18-theme-palette-openai-1780145642914/trace.jsonl`
  seq 11 confirms the catppuccin override applied; the rendered
  notification body reads `catppuccin`.
- **Why it matters:** the rule applies to **any** cell foreground,
  regardless of variant. Two of the three named variants violate
  their own design contract.
- **Provider:** all three (palette is process-wide).

### F5 — additional `Color::White` violations beyond already-filed tickets (NEW; medium)

- **Ticket:** `squeezy-u5w6`.
- **Rubric:** Visual clarity.
- **File:line — surfaces not covered by squeezy-syp / squeezy-23a /
  squeezy-9ui / squeezy-9k9 / squeezy-3j5 / squeezy-4zsv /
  squeezy-lbd9:**
  - `crates/squeezy-tui/src/overlay.rs:245` — slash help overlay
    non-selected rows.
  - `crates/squeezy-tui/src/prompt_queue.rs:167` — prompt queue
    non-selected rows.
  - `crates/squeezy-tui/src/streaming_patch.rs:295` — streaming
    apply_patch path label (`✎ <path>`).
  - `crates/squeezy-tui/src/lib.rs:4301` — MCP elicitation modal
    non-selected options.
  - `crates/squeezy-tui/src/lib.rs:4426` — request_user_input
    freeform answer-entry text.
  - `crates/squeezy-tui/src/lib.rs:4639` — app-notification body
    text.
  - `crates/squeezy-tui/src/lib.rs:7282, 7319, 7422, 7443, 7462,
    7492, 7525, 7542` — tool-card summary labels (~8 sites — every
    tool call adds a `Color::White` summary span).
- **Why these matter:** the tool-card summary labels are the most
  frequently rendered violation: every successful tool call paints at
  least one `Color::White` span in the transcript card. The overlay
  and queue rows surface the same defect as the approval menu
  (squeezy-9ui) on adjacent surfaces.
- **Provider:** all three.

## Findings already filed by sibling agents (reference, no new ticket)

| Ticket | Surface | Notes |
|---|---|---|
| `squeezy-syp` | startup banner `>_ Squeezy v…`, directory / languages value rows (`crates/squeezy-tui/src/lib.rs:5518, 5531, 5537`) | `Color::White` (lum 255) on the first frame. |
| `squeezy-3j5` | broader sweep across banner + user prompt + resume picker | catches ~32 `Color::White` occurrences. |
| `squeezy-23a` | `/config` overlay, 14 spans `Color::White` | covered by sibling agent. |
| `squeezy-9ui` | approval menu rows (`crates/squeezy-tui/src/lib.rs:4406, 4463`) | identical pattern to F5 list-item rows. |
| `squeezy-9k9` | approval preview body (`plain_white`) | `crates/squeezy-tui/src/approval.rs:215-220`. |
| `squeezy-8dd` / `squeezy-6fi` | `DIFF_DEL_FG = Rgb(252,165,165)` lum 191 | diff removed-line foreground. |
| `squeezy-4zsv` | request_user_input choice labels `Color::White` (lum 255) | plan-mode modal. |
| `squeezy-lbd9` | request_user_input Answer label `Color::Indexed(33)` (bright blue) | freeform modal label. |
| `squeezy-5ce` | Portkey provider not configured for wave-2 | aborts the third leg; medium per hard rules. |

## Probe summary

The probe asked four questions:

1. **Does `/theme dark` actually flip the runtime palette mid-session?**
   In this historical run, yes when driven through the `TuiHarness` composer
   (`send_keys`), and no when driven through the then-current eval
   `slash_command` path. Current `drive_tui` slash-command dispatch routes
   through the live TUI harness, so reruns should prefer the documented
   `slash_command` action unless they specifically need key-level coverage.

2. **Does the cache invalidate on every swap?** `palette_generation()`
   (`crates/squeezy-tui/src/render/palette.rs:108-120`) bumps inside
   `set_palette_tone_override` and `set_accent_variant` when the
   encoded value actually changes. `set_palette_tone_override` skips
   the bump on no-op writes (good); same for `set_accent_variant`.
   The three-swap probe forces at least two real transitions
   (`Default → Catppuccin → Default`) so the counter advances twice.
   No regression observed.

3. **Does AMBER `Rgb(215, 147, 52)` reach the banner / balls / spinner
   on the default theme?** Yes by static read of
   `crates/squeezy-tui/src/lib.rs:5044, 5567, 7070, 9120-9158`.
   AMBER is the spinner colour, the working-card "Working" label,
   the `Asked` role marker, and the idle/coin animation tones. The
   harness frame previews don't preserve foreground colour so this
   was confirmed by source inspection, not pixel assertion.

4. **Is any RGB hard-coded outside `palette.rs`?** No.
   `grep -rn "Rgb(" crates/squeezy-tui/src/` returns only
   `palette.rs`, `theme.rs`, and `theme_tests.rs`. The palette
   discipline holds; every accent goes through a named constant.

## Beads created in this domain

| ID | Title |
|---|---|
| `squeezy-ramu` | eval harness `/theme` writes to user's real `~/.squeezy/settings.toml` during scenario runs (major) |
| `squeezy-16k6` | TuiHarness hardcodes provider label as `eval-harness` instead of resolved provider name (medium) |
| `squeezy-8r6w` | WORKING_SHIMMER_HIGHLIGHT Rgb(255,251,235) luminance 250 violates dark-only cap (medium) |
| `squeezy-ybo8` | AccentVariant::Catppuccin mauve and HighContrast yellow violate dark-only cap (medium) |
| `squeezy-u5w6` | Color::White violations in overlay / prompt_queue / streaming_patch / MCP / notification / tool-card surfaces (medium) |
