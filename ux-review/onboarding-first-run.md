# Onboarding & First Run

> The path a brand-new user walks from `install.sh` through `squeezy doctor`, the first-run setup picker, and into an empty TUI.

**How it works today:** The installer drops the binary into `$HOME/.local/bin` and prints a PATH hint when needed. On first interactive launch with no saved model selection, a TUI setup picker walks theme → provider → (deferred key) → model → reasoning effort, persisting the choice to `settings.toml`; when the user defers a provider key it auto-opens the in-TUI config screen on the Models section. `squeezy doctor` is the advertised diagnostic. Once setup finishes, the user lands in the main TUI with an empty composer and a settle-delayed first-run hint that teaches the command-palette chord.

## Quick wins
- [Verify the binary runs before telling the user it's installed](#1-installer-claims-success-and-says-run-squeezy---help-without-verifying-path)
- [Surface `squeezy auth` and OAuth in the README and onboarding](#2-auth-subcommands-are-absent-from-the-readme-and-onboarding)
- [Let the setup picker accept a key instead of only deferring it](#3-the-key-step-can-only-defer-never-enter-a-key)
- [Close the doctor guidance loop with a one-line footer](#5-doctor-output-ends-with-no-next-step)
- [Disambiguate "/config" in the picker's deferral copy](#6-config-in-picker-copy-is-ambiguous)

## Findings

### 1. Installer claims success and says "run `squeezy --help`" without verifying PATH

- **Category · Severity · Effort:** Friction · High · S
- **Today:** After moving the binary into place, `install.sh` checks whether `$INSTALL_DIR` is on `PATH` and, if not, prints an `export PATH=...` hint. It then *unconditionally* prints `run 'squeezy --help' to get started` — even when the directory is not on PATH, so the very next thing the user is told to do fails with a bare `command not found`.
- **Friction:** The success message and the "command not found" reality contradict each other. A user who runs the one-liner in a shell whose rc file isn't sourced gets a closing instruction that is guaranteed to fail, with no link back to the PATH hint printed a few lines above.
- **Polish:** After install, run `"$INSTALL_DIR/squeezy" --version` in a subshell. On success, print "Installed and ready — run `squeezy --help`." On a PATH miss, gate the `--help` line behind the PATH hint: "squeezy is installed at `<dir>` but not on your PATH yet. Add the export above to your shell rc, then run `squeezy --help`."
- **Refs:** `install.sh:177-186`

### 2. Auth subcommands are absent from the README and onboarding

- **Category · Severity · Effort:** Discoverability · High · S
- **Today:** The CLI ships a full `auth` surface — `squeezy auth set <provider>`, `auth status`, `auth list`, plus OAuth flows (`auth anthropic login`, `auth openai-codex login`, `auth github-copilot login`). None of it appears in the README Quickstart, which only shows the `export OPENROUTER_API_KEY=...` env-var path. The doctor credential warning *does* name `squeezy auth set <provider>`, but a user only sees that after a check already failed.
- **Friction:** A new user who needs to add a key never learns the command exists unless they run `squeezy auth --help` on a hunch. They default to hand-editing `settings.toml`, which is error-prone and bypasses the OAuth subscription flows entirely.
- **Polish:** Add an auth line to the README Quickstart: "Add a key with `squeezy auth set openai`, check resolution with `squeezy auth status`, or use a subscription via `squeezy auth anthropic login` / `squeezy auth github-copilot login`." Mention the same commands wherever the setup picker defers a key.
- **Refs:** `README.md:52-69`, `crates/squeezy-cli/src/auth.rs:178-247`, `crates/squeezy-cli/src/doctor.rs:780-784`

### 3. The Key step can only defer, never enter a key

- **Category · Severity · Effort:** Friction · High · M
- **Today:** When the selected provider needs a credential, the picker shows a dedicated "Add provider key" step — but its only choice is `Configure <ENV_VAR> later in /config`. There is no field to paste a key during setup; the single option just defers it. The picker then auto-opens the config screen later, but the most natural first-run action (enter my key now) is impossible on the step literally titled "Add provider key."
- **Friction:** A user who arrives with their API key in the clipboard is funneled into a step that promises key entry and delivers only postponement, then bounced into a separate TOML-backed config panel. The step's title and its single deferral option contradict each other.
- **Polish:** Either let the Key step capture the key inline (write it through the same path `squeezy auth set` uses), or rename the step to set the right expectation (e.g. "Provider key" with options "Paste a key" / "Configure later") so the title stops promising entry it can't perform.
- **Refs:** `crates/squeezy-tui/src/startup_model_picker.rs:106,568-574`

### 4. Empty TUI offers no "type a prompt" guidance

- **Category · Severity · Effort:** Clarity · High · M
- **Today:** With an empty input, the composer renders a bare cursor span and nothing else — no placeholder, no ghost text. The only onboarding signal is the first-run hint strip, which is suppressed for a 700ms settle window and, by priority, teaches the command-palette chord first; it never says "type a prompt and press Enter."
- **Friction:** A brand-new user faces a blank composer under a decorative horizon rule with no statement of the primary action. Whether to type natural language, a slash command, or `/help` is left to guesswork until a hint fades in — and the first hint that does fade in is about the palette, not about sending a prompt.
- **Polish:** Show dim placeholder text in the empty composer ("Type a prompt and press Enter · Ctrl+P for commands") that clears on the first keystroke, reusing the existing dim-span styling. This is a static placeholder, independent of the settle-delayed hint engine.
- **Refs:** `crates/squeezy-tui/src/lib.rs:41817-41819`, `crates/squeezy-tui/src/first_run_hints.rs:93-99`

### 5. Doctor output ends with no next step

- **Category · Severity · Effort:** Smoothness · Medium · S
- **Today:** `DoctorReport::print` emits a header, a `version=… target=…` line, and the aligned check rows, then stops. On all-green there is no "you're ready, run `squeezy`"; on failure there is no pointer to troubleshooting, only a nonzero exit code.
- **Friction:** Users run `doctor` as a verification gate but can't tell whether it's a prerequisite or advisory, and a green run never tells them what to do next. A red run drops them with an exit code and no path to a fix.
- **Polish:** Add one footer line keyed off the summary state: all-ok → "Ready. Run `squeezy` to start."; warnings/failures → "See TROUBLESHOOTING.md (crates/squeezy-skills/external-docs/) for fixes." Skip the footer in `--json` mode.
- **Refs:** `crates/squeezy-cli/src/doctor.rs:164-197`

### 6. "/config" in picker copy is ambiguous

- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** The picker's deferral strings read `Configure <ENV_VAR> later in /config` for keys and `Configure in config` for themes. `/config` is not a file or a slash command the user types into a shell — it's the in-TUI config screen the picker auto-opens.
- **Friction:** "/config" reads like a path, URL, or shell command. A user may go hunting for a `config` file in their home directory, or type `/config` somewhere it does nothing, when the screen is in fact opened for them automatically after setup.
- **Polish:** Replace with intent-revealing copy: "Set `<ENV_VAR>` later — the config screen opens after setup" (key) and "Pick a theme later in the config screen" (theme). Drop the bare `/config` token.
- **Refs:** `crates/squeezy-tui/src/startup_model_picker.rs:573`, `crates/squeezy-tui/src/startup_model_picker.rs:252`

### 7. Setup picker footer gives every verb equal weight

- **Category · Severity · Effort:** Consistency · Medium · M
- **Today:** The footer always prints `↑/↓ move  ←/→ question  Enter <verb>  Esc/Q quit`. The Enter verb is context-dependent (apply/continue/model/effort/confirm) and is the only actionable button per step, but it sits visually flush with the navigation hints.
- **Friction:** A first-time user scanning for "what do I press now" has to read four equally-weighted hint groups to find the one that advances the flow. The primary action doesn't stand out from the always-present navigation keys.
- **Polish:** Emphasize the Enter action (bold or accent the verb, not just the "Enter" label) and demote `↑/↓` and `←/→` to a quieter secondary group. Keep all keys discoverable, but make the step's primary action the obvious focal point.
- **Refs:** `crates/squeezy-tui/src/startup_model_picker.rs:594-619`

### 8. Progress label denominator shifts as the cursor moves

- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** The header shows `Question N/M`, where `M = visible_steps().len() + trailing_question_count`. `visible_steps()` is recomputed every frame from the *currently highlighted* provider and model: the Key step is counted only when the highlighted provider needs a key, and the Reasoning step only when the highlighted model supports it. So moving the cursor across providers/models on the Provider or Model step silently changes the total. The `trailing_question_count` (the separate resume picker shown later, outside this picker) is also folded into `M`.
- **Friction:** The denominator moving while the user only changed a highlight makes the progress feel unstable or injected, and counting a downstream resume question the user hasn't reached inflates the total against the steps actually visible here.
- **Polish:** Label this picker's own steps independently (e.g. `Step N of <setup steps>`) and stop folding the trailing resume question into the same fraction; surface that as its own subsequent question. If a dynamic denominator is kept, freeze it for the setup phase so a highlight change doesn't reflow the count.
- **Refs:** `crates/squeezy-tui/src/startup_model_picker.rs:188-207`

## Dropped from the draft

- **"Explain next steps when provider key setup is deferred"** — the premise (user enters the TUI with no breadcrumb) is contradicted by the code: deferring a key sets `open_model_config`, which becomes `open_config_section = Some(Models)` and auto-opens the config screen on TUI entry (`crates/squeezy-cli/src/main.rs:4274-4282`, `crates/squeezy-tui/src/lib.rs:46171-46180`). The remaining valid idea (surface `squeezy auth set`) is folded into finding 2.
- **"Clarify 'No choices available' error path"** — effectively unreachable for the described user. `detect_provider_choices` unconditionally adds OpenAI, Anthropic, Gemini, and Azure with their curated model lists regardless of whether any key is set (`crates/squeezy-cli/src/main.rs:4285-4313,4497-4499`), so a fresh user with no env vars still sees four providers. The empty-list error the finding describes does not fire in practice.
- **"Hint about command palette does not show real key binding"** — false. `first_run_hint_message` resolves the chord live via `key_hint(app, keymap::Action::ToggleCommandPalette)` and substitutes it into `HintId::message`, so a rebound key shows the user's actual chord (`crates/squeezy-tui/src/lib.rs:13902-13915`, `crates/squeezy-tui/src/first_run_hints.rs:88-99`).
- **"No indication that setup questions are optional or can be skipped"** — the premise (Esc/Q discards progress and falls back to a saved config) is wrong. The picker only runs when no model selection is configured, and Esc/Q returns `None`, which exits the process cleanly (`crates/squeezy-tui/src/startup_model_picker.rs:324-325,372`, `crates/squeezy-cli/src/main.rs:913-916`). The footer's "quit" label is accurate; there is no saved-config fallback to surface.
