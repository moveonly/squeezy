# CLI & Headless Surface

> The flags, subcommands, and plain-text output users hit through `squeezy --prompt`, `doctor`, `config`, `auth`, `providers`, bundled `/help`, and the one-line `install.sh`.

**How it works today:** Top-level flags (`--prompt`, `--format`, `--prompt-permission-mode`, `--model-profile`, …) drive non-interactive runs, while `auth`, `providers`, `config`, and `doctor` subcommands print space-aligned text tables (or `--json`). Most error and help strings are hand-written and generally solid — several already carry quoting hints, valid-value lists, and schema docstrings. The remaining gaps are narrow polish: a few `--help` strings omit context that already exists in the source doc comments, a couple of empty-state tables dead-end instead of pointing at the next action, and the install script routes its PATH warning to stdout where redirects swallow it.

## Quick wins
- [Permission-mode `--help` omits the default and what each mode does](#1-prompt-permission-mode-help-omits-default-and-semantics)
- [`--format json` help never points at the documented event schema](#2-format-json-help-doesnt-reference-the-event-schema)
- [`providers list --configured` empty output is a dead-end](#3-providers-list---configured-empty-output-dead-ends)
- [Unsupported `/help` topic dumps a flat comma list instead of the grouped index](#4-unsupported-help-topic-dumps-a-flat-topic-list)
- [`install.sh` PATH warning prints to stdout, lost on redirect](#5-installsh-path-warning-routes-to-stdout)
- [`auth status` `ENV` column header is ambiguous](#6-auth-status-env-column-header-is-ambiguous)

## Findings

### 1. `--prompt-permission-mode` help omits default and semantics
- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** `squeezy --help` shows `Permission behavior for non-interactive --prompt runs: auto-approve-ask, deny-ask, or fail-on-ask`. The three names are listed with no indication of which is the default or what each does.
- **Friction:** The `ValueEnum` variants already carry precise doc comments (`AutoApprove` "Allow each permission request once… keeps CI prompts from hanging", `Deny` "let the agent continue with the denied tool result", `Fail` "make the CLI command fail immediately"), but clap's `help =` string overrides them, so none of that reaches `--help`. A user has to read the source to learn that `auto-approve-ask` is the default and that the three modes diverge on whether the turn continues.
- **Polish:** Fold the default and one-line semantics into the help string, e.g. `auto-approve-ask (default; approve each request once), deny-ask (deny but continue), fail-on-ask (deny and exit non-zero)`. Marking the variant via `default_value_t` already exists at line 145 — surface it in the text too.
- **Refs:** `crates/squeezy-cli/src/main.rs:78-88`, `crates/squeezy-cli/src/main.rs:141-148`

### 2. `--format json` help doesn't reference the event schema
- **Category · Severity · Effort:** Clarity · Medium · S
- **Today:** The flag help reads `Non-interactive output format for --prompt: 'default' (text deltas) or 'json' (one event per line). Experimental; schema may change.` It says "one event per line" but never names what an event is.
- **Friction:** The schema is in fact documented — the `PromptFormat` doc comment (lines 62-67) states the line shape follows the `LlmEvent` enum (`type` + `data`) in `crates/squeezy-llm/src/lib.rs`. A script author reading `--help` can't see that pointer and ends up reverse-engineering the stream.
- **Polish:** Add the schema reference to the help string: `…'json' emits one JSON LlmEvent per line (type + data; see squeezy-llm). Schema is experimental and may change.` The `--format json requires --prompt` guard error (line 787-790) is already clear, so this is purely the discoverability of the shape.
- **Refs:** `crates/squeezy-cli/src/main.rs:62-67`, `crates/squeezy-cli/src/main.rs:134-140`

### 3. `providers list --configured` empty output dead-ends
- **Category · Severity · Effort:** Feedback · Low · S
- **Today:** On a fresh install with no API keys set, `squeezy providers list --configured` prints `(no providers match the filter)` and stops.
- **Friction:** The message is technically correct but offers no next step. A new user can't tell whether nothing is configured (expected) or whether the install is broken, and gets no hint about how to configure a provider.
- **Polish:** When the empty result is caused by the `--configured` filter, swap in an actionable message, e.g. `(no providers configured yet) — set an API key with 'squeezy auth set <provider>' or add an inline api_key in settings.toml`. The `auth set` subcommand ("Store a provider API key as inline `api_key`") is the right thing to point at.
- **Refs:** `crates/squeezy-cli/src/providers.rs:81-83`, `crates/squeezy-cli/src/auth.rs:180-181`

### 4. Unsupported `/help` topic dumps a flat topic list
- **Category · Severity · Effort:** Clarity · Low · S
- **Today:** When a topic isn't found, `unsupported()` shows any "Did you mean" suggestions and then `Try one of these topics: <every topic id, comma-joined>.` — a single flat line of 20+ ids.
- **Friction:** A grouped, scannable index already exists in `topic_index()` (Getting started / Models and providers / Permissions and sandbox / …), but the not-found path ignores it and emits the raw flat list instead, which is the harder thing to read of the two.
- **Polish:** Keep the "Did you mean" line, then replace the flat dump with a pointer to the organized index: `Run '/help' with no argument to see all topics grouped by category.` This reuses the better-organized view instead of competing with it.
- **Refs:** `crates/squeezy-skills/src/help.rs:103-118`, `crates/squeezy-skills/src/help.rs:229-272`

### 5. `install.sh` PATH warning routes to stdout
- **Category · Severity · Effort:** Consistency · Low · S
- **Today:** `info()` prints to stdout; `err()` prints to stderr. The post-install PATH check (lines 180-184) and the closing `run 'squeezy --help'` line (186) both go through `info`/`printf` to stdout.
- **Friction:** A user running `curl … | sh > install.log` (or any stdout redirect) sends the "`$INSTALL_DIR` is not on your PATH yet" warning into the log file. They see nothing on the terminal, then hit `command not found` when they try `squeezy --help`. The most important onboarding message is the one most likely to be swallowed.
- **Polish:** Add a `warn() { printf 'install.sh: %s\n' "$*" >&2; }` helper and route the PATH-not-on-PATH branch (and the `export PATH=…` line) through it so the hint lands on the terminal even when stdout is redirected. The success lines can stay on stdout.
- **Refs:** `install.sh:41-48`, `install.sh:177-186`

### 6. `auth status` `ENV` column header is ambiguous
- **Category · Severity · Effort:** Consistency · Low · S
- **Today:** `auth status` renders a `PROVIDER | SOURCE | ENV | INLINE` table. The `ENV` cell holds a variable name — `OPENAI_API_KEY`, `OPENAI_API_KEY (set)`, or `ANTHROPIC_API_KEY (fallback set)` — depending on state. There is no legend.
- **Friction:** The header `ENV` reads as a status ("is it env-backed?"), but the cell is actually a variable name that sometimes carries a `(set)` / `(fallback set)` suffix. Users coming from tools that show a plain yes/no can misread an unset row (bare var name, no suffix) as "set."
- **Polish:** Rename the header to `ENV VAR` so the cell content reads as a name, and append a one-line legend to the output: `(set) = env var is set · (fallback set) = fallback env var is set`.
- **Refs:** `crates/squeezy-cli/src/auth.rs:1369-1399`

### 7. Empty-value rendering is inconsistent across subcommands
- **Category · Severity · Effort:** Consistency · Low · S
- **Today:** `providers list` and `providers info` render missing fields with `non_empty()`, which substitutes `(unset)`. `auth status` renders a missing inline key as `-`, and a missing env var as the bare variable name.
- **Friction:** Three sibling subcommands print "nothing here" three different ways (`(unset)`, `-`, bare name). A user scanning `providers` then `auth` output has to re-learn the empty-state convention each time, and `-` vs `(unset)` carry no semantic difference to justify the split.
- **Polish:** Pick one empty-state token (`(unset)` reads clearest) and use it across the provider and auth tables for genuinely-absent values. Keep the meaningful `(set)` / `(fallback set)` suffixes in `auth status`, which convey real state rather than emptiness.
- **Refs:** `crates/squeezy-cli/src/providers.rs:212-214`, `crates/squeezy-cli/src/auth.rs:1386-1397`

### 8. Duplicate `/parent` slash-command help entry
- **Category · Severity · Effort:** Consistency · Low · S
- **Today:** `SLASH_COMMAND_HELP_TABLE` contains two `/parent` entries with conflicting descriptions — "parent (headline) model, bypassing cheap-model routing" at lines 1142-1150 and "parent (non-cheap) model, overriding any active routing" at lines 1396-1404.
- **Friction:** User-facing impact is small because `answer_slash_command()` uses `.find()` (the first entry wins) and the drift-test consumers collapse names into a `HashSet`, so the duplicate is silently absorbed rather than shown twice. But the table now carries two competing source-of-truth descriptions for one command, which is a maintenance trap — an edit to one won't show up because the other shadows it.
- **Polish:** Delete the second `/parent` entry (lines 1396-1404), keeping the more detailed first one. Since the existing drift tests dedup via `HashSet` and can't catch this, add a debug-assert or unit test that `SLASH_COMMAND_HELP_TABLE` has no duplicate `name`s.
- **Refs:** `crates/squeezy-skills/src/help.rs:1142-1150`, `crates/squeezy-skills/src/help.rs:1396-1404`, `crates/squeezy-skills/src/help.rs:1418-1421`

### 9. `doctor` text output isn't ordered by severity
- **Category · Severity · Effort:** Clarity · Low · M
- **Today:** `DoctorReport::print()` emits a `squeezy: ok|fail|ok (warnings)` header, a `version=/target=` line, then one `  [ok|warn|fail] <name>  <detail>` row per check, in the fixed order they were pushed (config, repo_profile, provider, probe, update, workspace_paths, …).
- **Friction:** The `[fail]`/`[warn]` tag at the start of each row is already a good signal, but failing checks are interleaved by category rather than surfaced first, so a user skimming a run with several green checks has to scan every row to confirm there are no failures buried in the middle.
- **Polish:** Sort the rows for the human-readable path by status (fail, then warn, then ok) before printing, preserving category order within each group. The `--json` body should keep source order for stable parsing. This is the highest-value, lowest-risk readability change; full per-category section grouping is optional and not needed.
- **Refs:** `crates/squeezy-cli/src/doctor.rs:163-197`, `crates/squeezy-cli/src/doctor.rs:587-599`

---

**Dropped from the draft (premise didn't survive the code):**
- *"Install script does not hint at PATH issues on first success"* — the PATH hint already exists (install.sh:177-184) and prints before the closing line; the real problem is the output channel, covered by finding 5.
- *"Error message for invalid `--model-profile` does not hint at valid values"* — the error already says `expected cheap, balanced, or strong`, naming all three. The draft's proposed pointer (`squeezy providers list`) lists providers/models, not profiles, so it would mislead.
- *"Config explain field path parser errors are overly detailed / missing `:` hint"* — `split_config_field_path` only treats `.`/`"`/`'` specially; an unquoted `:` is not a parse error. `model_limits.openai:gpt-5.5…` falls through to the unknown-field branch (main.rs:1313-1318), which **already** tells the user to quote keys containing `.`. No proactive hint is missing.
- *"Config browse text output uses inconsistent spacing for section counts"* — `render_text` already pushes a `'\n'` between every section (config_browse.rs:88, 90, 92). The finding's own proposal concedes this; there is no missing blank line.
