# wave2-17 error-and-failure-messages

- **Domain:** 17 — error-and-failure-messages
- **Scenarios:**
  - `crates/squeezy-eval/fixtures/scenarios/wave2-17-error-messages-openai.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-17-error-messages-anthropic.toml`
  - `crates/squeezy-eval/fixtures/scenarios/wave2-17-error-messages-portkey.toml`
- **Run dirs (post-fix):**
  - openai: `target/eval/wave2-17-error-messages-openai-1780146037677`
  - anthropic: `target/eval/wave2-17-error-messages-anthropic-1780146098175`
  - portkey: `target/eval/wave2-17-error-messages-portkey-1780146137406` (provider-not-configured; see finding 5)
- **Pre-fix run dirs (cited in findings 1, 4):**
  - anthropic: `target/eval/wave2-17-error-messages-anthropic-1780145720238`
  - openai: `target/eval/wave2-17-error-messages-openai-1780145361023`

## Scenario shape

Each scenario drives three failure modes back-to-back in build mode:

1. `read_file` on `nonexistent.txt` — must surface a path-aware not-found error.
2. `shell` running `exit 7` — must report the numeric exit code.
3. `read_file` with no `path` argument — must surface the dispatcher's missing-arg validation message.

The expected rubric is the wave-2 standard (visual clarity, functionality,
messaging, diff readability, progressive disclosure, cross-model
consistency).

## Defects

### Finding 1 — Anthropic Haiku 400s on every turn when `max_output_tokens ≤ 1024`

- **Provider:** anthropic (`claude-haiku-4-5-20251001`)
- **Severity:** major
- **Rubric:** Functionality, Cross-model consistency
- **Suspect:** `crates/squeezy-llm/src/anthropic.rs:139-144`
- **Ticket:** `squeezy-6vjv`

Every turn in the pre-fix run failed with HTTP 400:
`thinking.enabled.budget_tokens: Input should be greater than or equal to 1024`
(see `target/eval/wave2-17-error-messages-anthropic-1780145720238/trace.jsonl`
seq 5, 11, 18). The OpenAI and Portkey siblings accept the same
`max_output_tokens = 1024` without issue, so this is a cross-model regression
local to the Anthropic adapter.

Root cause:

```rust
// crates/squeezy-llm/src/anthropic.rs:139
let budget =
    u64::from(effort.thinking_budget_tokens()).min(max_tokens.saturating_sub(1));
body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
```

With `max_tokens = 1024` and the default `Low` budget (`4096`), the result is
`min(4096, 1023) = 1023`, which Anthropic rejects. The Haiku-4.5 model has
`reasoning_effort = true` in `crates/squeezy-llm/src/models.json`, so the
thinking block is always emitted when an effort is configured.

Mitigated in the wave-2 scenario by bumping `max_output_tokens = 8192`; the
underlying floor still needs to land in the adapter (raise the budget floor
to `1024`, or skip the `thinking` block entirely when `max_tokens < 1025`).

### Finding 2 — inline-code markdown rendering uses bright Cyan (luminance ~178)

- **Provider:** openai, anthropic (palette is provider-agnostic — both render the same way)
- **Severity:** major
- **Rubric:** Visual clarity (palette guardrail)
- **Suspect:** `crates/squeezy-tui/src/render/markdown.rs:614` (and `:359` link case, `:604` LightMagenta case)
- **Ticket:** `squeezy-efng`

The post-fix runs' `frames.jsonl` records every inline-code span with
`fg = "cyan"`:

```
target/eval/wave2-17-error-messages-openai-1780146037677/frames.jsonl
  frame 0: fg=cyan text='nonexistent.txt'
  frame 1: fg=cyan text='exit 7'
  frame 2: fg=cyan text='read_file', fg=cyan text='path'

target/eval/wave2-17-error-messages-anthropic-1780146098175/frames.jsonl
  same shape, three frames, all inline-code is cyan.
```

`Color::Cyan` maps to `Rgb(0, 255, 255)` via
`crates/squeezy-tui/src/render/palette.rs:246`. Per the wave-2 brief
(`docs/internal/EVAL_COVERAGE_PLAN_WAVE2.md`), the luminance threshold is
`0.299·R + 0.587·G + 0.114·B ≤ 160`. Cyan luminance = `0·0.299 + 255·0.587 + 255·0.114 = 178.7` → **over budget by 18**.

Source — `inline_code_style_for`:

```rust
// crates/squeezy-tui/src/render/markdown.rs:599
fn inline_code_style_for(text: &str) -> Style {
    ...
    } else if lower.contains("model") || text.starts_with('@') {
        Color::LightMagenta              // RGB (255,128,255) luminance ~180 — also over budget
    } else if lower.contains("branch") || lower.contains("refs/") || lower.contains('/') {
        Color::Magenta                   // RGB (255,0,255) luminance ~105 — ok
    } else if lower.contains("cost") ...
        palette::AMBER                   // luminance ~135 — ok
    } else {
        Color::Cyan                      // default branch — luminance 178 — over budget
    };
}
```

Links share the same `Color::Cyan` (`render/markdown.rs:359`).

Because every assistant reply that names a path, command, or symbol passes
through this branch, the brand `AMBER` (luminance ~135) is dominated by the
cyan accent on the most-used surface in the TUI.

Fix: route the default branch through `palette::ACCENT_CYAN`
(`Rgb(64, 158, 158)`, luminance ~119) which already exists in
`palette.rs:47`. Same treatment for `LightMagenta` (introduce a dark variant).

### Finding 3 — turn-failed banner dumps verbatim provider JSON

- **Provider:** anthropic (most visible there; same banner path serves every provider)
- **Severity:** medium
- **Rubric:** Messaging
- **Suspect:** `crates/squeezy-llm/src/anthropic.rs:452`, `crates/squeezy-tui/src/lib.rs:10005-10019`
- **Ticket:** `squeezy-uwsj`

When Anthropic returned the 400 from finding 1, the user-visible status line
became:

```
provider request failed: 400 Bad Request: {"type":"error","error":{"type":"invalid_request_error","message":"thinking.enabled.budget_tokens: Input should be greater than or equal to 1024"},"request_id":"req_011CbYniQpxBV6jUCNnQEpHX"}; retry or check provider/network status
```

Three messaging problems:

1. The advice "retry or check provider/network status" is wrong — a 400 with
   `invalid_request_error` is non-transient; the retry loop will fail
   identically every time.
2. The provider's JSON body is dumped raw; the `error.message`
   ("thinking.enabled.budget_tokens: Input should be greater than or equal to 1024")
   is the only useful prose and could be lifted out.
3. The hint never names the offending squeezy config field
   (`max_output_tokens` would need to be ≥ 1025 to satisfy the floor).

Source:

```rust
// crates/squeezy-llm/src/anthropic.rs:452
let formatted = format!("{status}: {message}");
Err(SqueezyError::ProviderRequest(formatted))?;
```

```rust
// crates/squeezy-tui/src/lib.rs:10010
SqueezyError::ProviderRequest(_) | SqueezyError::ProviderStream(_) => {
    format!("{error}; retry or check provider/network status")
}
```

Fix sketch: parse the Anthropic 4xx body (`type`, `error.type`,
`error.message`, `request_id`) and surface the human prose; only suffix
"retry or check ..." when the status is 5xx / 429 / network.

### Finding 4 — eval `permission_mode` overlay does not cover the Read capability

- **Provider:** openai (and any provider — the harness shaping is provider-agnostic)
- **Severity:** medium
- **Rubric:** scenario-harness meta (orthogonal to the rubric dimensions; affects every scenario that probes tool-error UX without writing approve hooks)
- **Suspect:** `crates/squeezy-eval/src/driver.rs:476-485`
- **Ticket:** `squeezy-7c44`

Initial OpenAI run (`target/eval/wave2-17-error-messages-openai-1780145361023`)
had `[squeezy] permission_mode = "allow"`. Trace shows every `read_file`
call still gated:

```
seq 8:  approval request: read_file → denied_no_action
seq 9:  tool_call_completed: read_file status=Denied (user denied tool call; capability=read target=workspace:*)
seq 122: same pattern on turn 3
```

`crates/squeezy-eval/src/driver.rs:476` applies the overlay's
`permission_mode` to `config.permissions.edit / shell / web / mcp` and skips
`read` and `ignored_search`. While `permissions.read` defaults to `Allow`,
any installation that tightens `read = "ask"` cannot override it from the
scenario. More importantly, the brief expects scenarios to be the source of
truth for what mode the agent runs in.

The wave-2 scenarios were updated to pre-arm explicit `approve` actions for
`read_file` so the underlying not-found / arg-validation errors actually
land. Once the harness is fixed, those `approve` steps can be dropped.

### Finding 5 — Portkey provider not configured in this eval host

- **Provider:** portkey (`@openrouter/qwen/qwen3.6-35b-a3b`)
- **Severity:** medium (per the wave-2 brief)
- **Rubric:** Cross-model consistency (third-provider audit unverified)
- **Suspect:** environment / settings provisioning, not a code defect
- **Ticket:** `squeezy-ge38`

The Portkey scenario aborts immediately:

```
provider is not configured: missing PORTKEY_API_KEY or SQUEEZY_PORTKEY_KEY;
set the env var or add [providers.<name>] api_key = "…" to
~/.squeezy/settings.toml or the project-local settings.toml
```

Per the wave-2 brief, a provider config error is recorded as a `medium`
finding rather than aborting the agent. The third leg of the cross-provider
audit (does Qwen3-via-Portkey render concrete next-step hints on tool
failures the way OpenAI and Anthropic do) remains open until the key is
provisioned.

The error message itself is the gold standard for this domain — concrete
(names both env var spellings and the settings.toml shape) and actionable
(tells the operator exactly which file to edit).

## What's working well

- **OpenAI and Anthropic (post-fix) error messaging.** Both providers produce
  one-line summaries that name the path, the exit code, the missing
  argument, and end with a next-step hint (verify the path, fix the script,
  supply the missing arg). The assistant text drives this; the LLM is
  doing the work the rubric asks for.
- **Tool-error card colour.** `status_color(ToolStatus::Error)` resolves to
  `ERROR_RED = Rgb(180, 60, 60)` (`crates/squeezy-tui/src/lib.rs:9044`),
  luminance ~106. The `BANG_RED` cousin `Rgb(153, 27, 27)` (luminance ~73)
  is reserved for `!`-shell prompts. Both are inside the dark-only
  guardrail.
- **`read_file` not-found error text.** `crates/squeezy-tools/src/lib.rs:3896`
  emits `"path does not exist or is inaccessible: <io_err>"` which is
  concrete (cites the source error) without dumping raw IO debug info.
- **Tool-args validation.** `read_file` without `path` returns
  `"invalid tool arguments: missing field 'path'"` — concrete + names the
  missing field.

## Reproduction

```sh
source ~/.env.sh
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-17-error-messages-openai.toml \
  --no-triage
cargo run -p squeezy-eval --quiet -- run \
  crates/squeezy-eval/fixtures/scenarios/wave2-17-error-messages-anthropic.toml \
  --no-triage
# Portkey: provision PORTKEY_API_KEY first, then run the third scenario.
```

Inspect each run dir's `frames.jsonl` for `styled_lines.spans[*].fg` to
confirm the bright-cyan rendering (finding 2). Inspect
`target/eval/wave2-17-error-messages-anthropic-1780145720238/trace.jsonl`
for the unrecoverable provider 400 (findings 1, 3) — the pre-fix run is
preserved as evidence.
