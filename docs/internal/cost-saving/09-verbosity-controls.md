# Verbosity Controls

## Motivation

Different tasks need different output budgets. A scripted edit wants
the assistant to do the work, report `done`, and stop. A complex
investigation wants full disclosure — hypothesis, calls tried,
conclusion. Forcing one default across both wastes tokens in the
short case and starves the long case.

Squeezy splits the dial three ways. Assistant text is governed by
`ResponseVerbosity` (`/verbosity`). Inline preview of tool output in
the transcript is governed by `ToolOutputVerbosity` (`/tool-verbosity`).
Unified-diff stdout from shell commands has its own switch,
`ShellDiffInline` (`tui.shell_diff_inline`), because `git diff` is the
one tool whose value collapses if you head/tail-cap it. Each setting is
session-scoped. Response verbosity can change the provider request
(`text_verbosity` or a short prompt fragment); tool-output verbosity and
shell-diff folding control transcript/TUI rendering and do not shrink
provider request bytes.

## Mechanism

### The three enums

All three enums live next to the rest of the TUI config:

```rust
// crates/squeezy-core/src/lib.rs:6467-6483
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseVerbosity {
    Concise,
    Normal,
    Verbose,
}

impl ResponseVerbosity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Concise => "concise",
            Self::Normal => "normal",
            Self::Verbose => "verbose",
        }
    }
}
```

```rust
// crates/squeezy-core/src/lib.rs:6485-6501
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputVerbosity {
    Compact,
    Normal,
    Verbose,
}
```

```rust
// crates/squeezy-core/src/lib.rs:6503-6523
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellDiffInline {
    /// Render unified-diff output from shell commands in full, bypassing the
    /// collapsed-card head/tail preview cap. Default — a `git diff` card is
    /// only useful when every hunk is visible.
    Full,
    /// Keep shell-produced diffs on the same head/tail preview budget as
    /// other shell output. For users who run `git diff` against large files
    /// often enough that uncapped inline diffs overwhelm the transcript.
    Folded,
}
```

The docstrings carry the design call: default `Full` because head/tail
truncation discards every intermediate hunk; `Folded` is an opt-in for
users whose diffs are big enough to overwhelm the transcript.

### Defaults

The defaults live in `TuiConfig::from_settings`, which converts the
parsed `TuiSettings` into the concrete config the agent reads every
turn.

```rust
// crates/squeezy-core/src/lib.rs:6728-6753
response_verbosity: settings
    .response_verbosity
    .unwrap_or(ResponseVerbosity::Normal),
tool_output_verbosity: settings
    .tool_output_verbosity
    .unwrap_or(ToolOutputVerbosity::Compact),
...
shell_diff_inline: settings.shell_diff_inline.unwrap_or(ShellDiffInline::Full),
```

Out of the box: assistant output is `Normal`, tool-output previews
are `Compact` (smallest), shell diffs render in full.

### Slash command parsing

`/verbosity` and `/tool-verbosity` are typed `DispatchCommand` variants
parsed in `squeezy-agent/src/dispatch.rs` — first whitespace token
becomes the optional argument. `/diff` is bare; it captures a worktree
snapshot, not a verbosity toggle. The shell-diff folding switch lives
at `tui.shell_diff_inline` in settings (exposed via `/options`,
hot-reloaded via `set_shell_diff_inline`).

```rust
// crates/squeezy-agent/src/dispatch.rs:301,389-394
"/diff" => Self::Diff,
...
"/verbosity" => Self::Verbosity {
    value: first_token(rest),
},
"/tool-verbosity" => Self::ToolVerbosity {
    value: first_token(rest),
},
```

### TUI handlers

The TUI routes both typed commands to near-identical session mutators.
Bare form prints the current value plus a usage hint; with-arg form
parses, validates, and writes the new value into the live agent config.

```rust
// crates/squeezy-tui/src/lib.rs:3324-3344
fn handle_slash_verbosity(app: &mut TuiApp, agent: &mut Agent, value: Option<&str>) {
    let Some(raw) = value else {
        let current = agent.config_snapshot().tui.response_verbosity;
        app.status = format!("response verbosity: {}", current.as_str());
        app.push_transcript_item(TranscriptItem::system(format!(
            "response verbosity = {}\nusage: /verbosity [concise|normal|verbose]",
            current.as_str()
        )));
        return;
    };
    let Some(verbosity) = parse_response_verbosity(raw) else {
        app.status =
            format!("unknown response verbosity {raw:?}; expected concise, normal, or verbose");
        return;
    };
    app.response_verbosity = verbosity;
    let mut next = agent.config_snapshot();
    next.tui.response_verbosity = verbosity;
    agent.replace_config(next);
    app.status = format!("response verbosity → {}", verbosity.as_str());
}
```

`handle_slash_tool_verbosity` (lines 3349-3368) has the same shape
against `tui.tool_output_verbosity`. The mutation lands through
`agent.replace_config(next)` — the next turn's `LlmRequest` reads
from the updated snapshot.

The shell-diff switch uses a different pattern because the render path
consults it from deeply nested formatters that do not hold the agent
config. The TUI mirrors the live setting into an `AtomicU8` at startup
and on every settings hot-reload.

```rust
// crates/squeezy-tui/src/lib.rs:146-167
/// Process-wide override for `tui.shell_diff_inline`, pinned by the TuiApp
/// at startup and re-applied on settings hot-reload. Encoded as `0 = Full
/// (default)`, `1 = Folded`. A static lets the deeply-nested render path
/// consult the setting without threading it through every formatter, the
/// same pattern the palette uses for tone/accent overrides.
static SHELL_DIFF_INLINE_OVERRIDE: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

fn shell_diff_inline_setting() -> ShellDiffInline {
    match SHELL_DIFF_INLINE_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => ShellDiffInline::Folded,
        _ => ShellDiffInline::Full,
    }
}
```

### From setter to wire

`ResponseVerbosity` reaches the provider one of two ways depending on
whether the active model declares the `text_verbosity` capability in
`crates/squeezy-llm/src/models.json`. The split happens at
request-build time:

```rust
// crates/squeezy-agent/src/lib.rs:1962-1989
let native_text_verbosity = capabilities_for(self.provider.name(), &self.config.model)
    .is_some_and(|capabilities| capabilities.text_verbosity);
let raw_instructions = instructions_with_response_verbosity(
    &self.config.instructions,
    self.config.tui.response_verbosity,
    native_text_verbosity,
);
...
response_verbosity: request_response_verbosity(&self.config, self.provider.name()),
```

`request_response_verbosity` returns `None` for providers without
native support, so the LLM-level field is omitted:

```rust
// crates/squeezy-agent/src/lib.rs:4362-4369
fn request_response_verbosity(
    config: &AppConfig,
    provider_name: &str,
) -> Option<ResponseVerbosity> {
    capabilities_for(provider_name, &config.model)
        .filter(|capabilities| capabilities.text_verbosity)
        .map(|_| config.tui.response_verbosity)
}
```

`instructions_with_response_verbosity` is the prompt-side path. It
skips when the API parameter will carry the signal natively, and
skips when the value is the implicit default — the common path pays
zero tokens for the feature.

```rust
// crates/squeezy-agent/src/lib.rs:4525-4547
fn instructions_with_response_verbosity(
    instructions: &str,
    verbosity: ResponseVerbosity,
    native_text_verbosity: bool,
) -> String {
    // Cost-first: skip the prompt-side hint when the model already
    // accepts the `text.verbosity` API parameter (one signal is enough)
    // and when the value is the implicit default (Normal). This keeps
    // the system prompt lean on the common path.
    if native_text_verbosity || verbosity == ResponseVerbosity::Normal {
        return instructions.to_string();
    }
    let guidance = match verbosity {
        ResponseVerbosity::Concise => {
            "Response verbosity: concise. Prefer short, direct answers unless the task requires detail."
        }
        ResponseVerbosity::Verbose => {
            "Response verbosity: verbose. Include fuller rationale, context, and verification details when useful."
        }
        ResponseVerbosity::Normal => unreachable!("handled above"),
    };
    format!("{instructions}\n\n{guidance}")
}
```

So for `Concise` on a non-native provider, the system prompt gains
exactly one paragraph:

> Response verbosity: concise. Prefer short, direct answers unless
> the task requires detail.

For `Concise` on a native provider (OpenAI Responses, recent GPT
families per `models.json`), the prompt is unchanged and
`LlmRequest::response_verbosity = Some(Concise)` rides on the API.

`ToolOutputVerbosity` and `ShellDiffInline` are TUI-render-time
settings — they do not appear in the `LlmRequest`. They control how
much of a tool's stdout is rendered inline. The byte budgets per
level:

```rust
// crates/squeezy-tui/src/lib.rs:123-125
const TOOL_PREVIEW_COMPACT_BYTES: usize = 300;
const TOOL_PREVIEW_NORMAL_BYTES: usize = 1_200;
const TOOL_PREVIEW_VERBOSE_BYTES: usize = 4_000;
```

And the preview-line cap (with `usize::MAX` for the verbose escape
hatch):

```rust
// crates/squeezy-tui/src/lib.rs:7981-7987
fn tool_preview_line_cap(...) -> usize {
    let packet_cap = match verbosity {
        ToolOutputVerbosity::Compact => 3,
        ToolOutputVerbosity::Normal => 5,
        ToolOutputVerbosity::Verbose => usize::MAX,
    };
```

A 4 KiB stdout from `cargo test` collapses to ~300 bytes (Compact),
~1.2 KB (Normal), or renders in full up to 4 KB (Verbose). The full
text is always available off-card via `read_tool_output`.

The shell-diff path bypasses the cap when the setting agrees:

```rust
// crates/squeezy-tui/src/lib.rs:6948-6958
fn tool_bypasses_preview_cap_for_tool(tool: &ToolTranscript) -> bool {
    if tool_bypasses_preview_cap(tool.result.tool_name.as_str()) {
        return true;
    }
    if matches!(shell_diff_inline_setting(), ShellDiffInline::Full)
        && shell_output_is_unified_diff(tool)
    {
        return true;
    }
    false
}
```

## Worked example

A user starts a session with the defaults — `Normal` / `Compact` /
`Full` — and runs a long-form review of a fresh PR:

```
/verbosity verbose
```

The status line confirms `response verbosity → verbose`. The next
turn's `LlmRequest` build sees `response_verbosity = Verbose`. On
OpenAI it sets `response_verbosity: Some(Verbose)` on the request and
leaves the system prompt alone. On Anthropic
(`text_verbosity = false`), it leaves the API field omitted and
appends the verbose paragraph to the system prompt:

> Response verbosity: verbose. Include fuller rationale, context, and
> verification details when useful.

The review comes back full-fat. The user now wants a quick sanity
test:

```
/verbosity concise
/tool-verbosity compact
```

On a native-text-verbosity provider the API field flips to
`Some(Concise)` and the prompt is once again the bare
`self.config.instructions`. On a non-native provider, the prompt
swaps the verbose paragraph for the concise one ("Prefer short, direct
answers unless the task requires detail").

The model answers `cargo test` with a one-line summary. The transcript
card for `cargo test` now collapses to the Compact head-tail cap of
`3` lines and the byte budget of `300`, so a 4 KB compile log becomes
a `…` ellipsis sandwiched between three head and three tail lines.
The full output stays on disk in the session log; if the model needs
it later, `read_tool_output` returns it without re-streaming.

If the user runs `git diff` during this session, the unified-diff
detection in `shell_output_is_unified_diff` plus the default
`ShellDiffInline::Full` causes the diff to render in full, bypassing
the preview cap entirely — Compact for arbitrary noisy stdout, Full
for the one stream where compaction defeats the purpose.

## Edge cases & limits

**Cache interaction.** The provider's prompt cache hashes the prefix
of the system instructions (and on Anthropic, the tools array and the
user message list up to a breakpoint marker — see `cache_policy.rs`).
When the verbosity guidance text is appended to the system prompt, it
changes the hash and would normally invalidate the cached prefix.
Squeezy mitigates this two ways: native-capable providers omit the
prompt-side hint entirely, so the cached system prefix is unchanged
across `/verbosity` toggles and only the top-level
`response_verbosity` API field flips; non-native providers pay a
one-time cache miss on the turn after the switch, and subsequent
turns at the new verbosity reuse the new cached prefix. The
implicit-default short-circuit means a user who never touches
`/verbosity` pays no invalidation overhead at all.

**Pinned context awareness.** Conversation compaction (chapter 02)
treats pinned summaries as load-bearing and re-attaches them to the
post-compaction context. The verbosity guidance is appended to the
system instructions, not to the conversation, so compaction does not
see it as content to preserve — it is regenerated on every request
from the current `config.tui.response_verbosity` value. The layering
is clean: compaction owns conversation history; the verbosity layer
owns the per-turn instructions wrapper.

**Mid-session changes.** `/verbosity` and `/tool-verbosity` are
session-scoped — the new value applies on the next turn and persists
for the session, but does not write through to `settings.toml`. To
persist across runs the user edits the value via `/options`.
`SHELL_DIFF_INLINE_OVERRIDE` updates synchronously on hot-reload, so
re-rendering with a different folding choice does not require a
restart. Switching verbosity does not retroactively edit an in-flight
turn or already-rendered tool cards — the agent reads the value at
request-build time and the provider sees the new value at the next
turn boundary.

## Cost intuition

`Concise` vs `Verbose` on equivalent tasks tends to swing output
tokens 30–60%. Providers with a native `text_verbosity` parameter
typically tighten response shape more aggressively than they would
from a system-prompt hint alone, so the saving runs higher on native
providers; on non-native providers the appended paragraph is a strong
steer but not a hard cap.

`ToolOutputVerbosity` does not save provider tokens directly — the
full tool output already lives in the conversation history regardless
of how the TUI renders it. The savings come downstream: a Compact
transcript renders less, the user scrolls less, and when compaction
runs the inline tool-result blocks the user has already glanced past
have been re-summarized by the receipt-stub layer (chapter 03).

`ShellDiffInline::Folded` is the single biggest saver on a transcript-
bytes basis. A `git diff main..HEAD` against a moderate PR can
produce 10–50 KB of stdout. `Full` renders all of it; `Folded` caps
it at the per-verbosity preview budget (300 / 1 200 / 4 000 bytes).
Users who pipe `git diff` into the assistant often save more from
this one switch than from the response-verbosity change.

The three controls compose. A "quick scripted edit" session runs
Concise + Compact + Folded; a "deep review" session runs Verbose +
Normal + Full. The user picks the shape when it becomes visible —
no forecasting required.
