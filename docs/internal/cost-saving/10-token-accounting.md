# Token Accounting and Context Telemetry

## Motivation

Every other cost-saving mechanism in this codebase is downstream of one
question: *where are the tokens going?* Compaction needs to know when the
budget is filling. Verbosity controls need to show the user which knobs will
help. Prompt caching needs to report how many tokens were served from cache
versus billed at full rate. A user who suspects their bill is high needs a
per-source attribution, not a single opaque "tokens used" number.

Squeezy maintains two parallel views per turn. The *provider* view is what
the API actually billed, with cache hits and cache writes broken out. The
*local* view is a deterministic estimate of the request payload by category
(user/assistant text, function-call args, tool outputs, reasoning, images).
The `/context` slash command shows both. Without that breakdown, compaction
triggers fire blind and the user cannot tell whether `/tool-verbosity`,
`/verbosity`, or attachment trimming is the right intervention.

## Mechanism

### `CostSnapshot` — the provider-facing tally

Every assistant turn produces a `CostSnapshot`. It is the agent's
cross-provider record of what the API charged.

```rust
// crates/squeezy-core/src/lib.rs:9804-9813
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub estimated_usd_micros: Option<u64>,
}
```

Each field maps to a billing line:

- `input_tokens`: prompt size the model saw, normalised so every provider
  reports the same convention (uncached + cached + cache-write combined).
  Without this fold-in, an Anthropic cache-hit turn would expose a tiny value
  — the provider's raw `usage` reports only the uncached delta — and any log
  reader would conclude the prompt was short.
- `output_tokens`: completion tokens at the output rate, including reasoning
  streamed back from the model.
- `reasoning_output_tokens`: the subset of `output_tokens` that was reasoning
  rather than visible text. Pure cost with no carry-forward on most providers.
- `cached_input_tokens`: prefix-cache hits, charged at ~10% of the standard
  input rate.
- `cache_write_input_tokens`: first-pass cache writes, charged at ~125%.
- `estimated_usd_micros`: dollar cost in microdollars, derived from the four
  token counters above and the per-rate pricing in `models.json`.

### Per-provider extraction

Each provider's streaming state translates that wire format into the common
`CostSnapshot`.

**Anthropic** is the explicit case. The SSE `message_delta` events carry a
`usage` object whose `input_tokens` field is the *uncached delta only*.
`merge_usage` (`crates/squeezy-llm/src/anthropic.rs:1044-1064`) writes
`input_tokens`, `output_tokens`, `cache_read_input_tokens`, and
`cache_creation_input_tokens` into the stream state on every chunk that
carries `usage`. The state's `cost()` folds them back together at snapshot
time:

```rust
// crates/squeezy-llm/src/anthropic.rs:744-756
let base = self.input_tokens;
let cache_read = self.cache_read_input_tokens.unwrap_or(0);
let cache_write = self.cache_creation_input_tokens.unwrap_or(0);
let total_input = base.map(|b| b.saturating_add(cache_read).saturating_add(cache_write));
CostSnapshot {
    input_tokens: total_input,
    output_tokens: self.output_tokens,
    reasoning_output_tokens: None,
    cached_input_tokens: self.cache_read_input_tokens,
    cache_write_input_tokens: self.cache_creation_input_tokens,
    estimated_usd_micros: None,
}
```

The completion event emits this snapshot at `message_stop`
(`crates/squeezy-llm/src/anthropic.rs:1024-1029`).

**OpenAI** uses the Responses API's `usage` object. The provider already
reports `input_tokens` as the total prompt, but it does not expose a
cache-write counter:

```rust
// crates/squeezy-llm/src/openai.rs:774-787
CostSnapshot {
    input_tokens: usage.get("input_tokens").and_then(Value::as_u64),
    output_tokens: usage.get("output_tokens").and_then(Value::as_u64),
    reasoning_output_tokens: usage
        .get("output_tokens_details")
        .and_then(|details| details.get("reasoning_tokens"))
        .and_then(Value::as_u64),
    cached_input_tokens: usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64),
    cache_write_input_tokens: None,
    estimated_usd_micros: None,
}
```

`reasoning_output_tokens` comes from `output_tokens_details.reasoning_tokens`
and `cached_input_tokens` from `input_tokens_details.cached_tokens`. The
explicit `cache_write_input_tokens: None` is load-bearing — OpenAI does not
distinguish a first write into the cache from an uncached miss on the wire,
so the cost estimator skips the cache-write term entirely for OpenAI.

**Bedrock** uses the typed AWS SDK rather than parsing JSON, but the shape is
identical:

```rust
// crates/squeezy-llm/src/bedrock.rs:420-429
if let Some(usage) = meta.usage {
    state.input_tokens = Some(u64::try_from(usage.input_tokens).unwrap_or(0));
    state.output_tokens = Some(u64::try_from(usage.output_tokens).unwrap_or(0));
    state.cache_read_input_tokens = usage
        .cache_read_input_tokens
        .and_then(|n| u64::try_from(n).ok());
    state.cache_write_input_tokens = usage
        .cache_write_input_tokens
        .and_then(|n| u64::try_from(n).ok());
}
```

Bedrock's `i32` counts are coerced into `u64`; `try_from(...).unwrap_or(0)`
defends against a hypothetical negative without aborting the stream.

**Google** parses `usageMetadata` on every chunk and overwrites the cost
state — Gemini emits running totals rather than deltas, so the latest chunk
wins:

```rust
// crates/squeezy-llm/src/google.rs:365-370
if let Some(usage) = value.get("usageMetadata") {
    cost.input_tokens = usage.get("promptTokenCount").and_then(Value::as_u64);
    cost.output_tokens = usage.get("candidatesTokenCount").and_then(Value::as_u64);
    cost.cached_input_tokens = usage.get("cachedContentTokenCount").and_then(Value::as_u64);
    cost.reasoning_output_tokens = usage.get("thoughtsTokenCount").and_then(Value::as_u64);
}
```

Gemini surfaces reasoning explicitly as `thoughtsTokenCount`. Like OpenAI it
has no cache-write counter.

### Cost estimation

`estimate_cost` turns a `CostSnapshot` into microdollars using the per-model
pricing table from `models.json`:

```rust
// crates/squeezy-llm/src/registry.rs:385-412
pub fn estimate_cost(provider: &str, model: &str, cost: &CostSnapshot) -> Option<u64> {
    let pricing = model_info_for(provider, model).and_then(|entry| entry.pricing)?;
    let cached_input_tokens = cost.cached_input_tokens.unwrap_or(0);
    let cache_write_input_tokens = cost.cache_write_input_tokens.unwrap_or(0);
    let standard_input_tokens = cost
        .input_tokens
        .unwrap_or(0)
        .saturating_sub(cached_input_tokens)
        .saturating_sub(cache_write_input_tokens);
    Some(
        estimate_tokens(standard_input_tokens, pricing.input_usd_micros_per_mtok)
            + estimate(cost.output_tokens, pricing.output_usd_micros_per_mtok)
            + estimate_tokens(
                cached_input_tokens,
                pricing.cache_read_usd_micros_per_mtok.unwrap_or(0),
            )
            + estimate_tokens(
                cache_write_input_tokens,
                pricing.cache_write_usd_micros_per_mtok.unwrap_or(0),
            ),
    )
}
```

Four terms: standard-rate input (the remainder after subtracting cached and
cache-write shares), output, cache-read, cache-write. The fold-in by
Anthropic and Bedrock makes the same arithmetic work for every provider
without per-provider branches. Per-rate fields default to `0` via
`unwrap_or(0)` when the pricing table omits them, so OpenAI's empty
`cache_write_input_tokens` produces no phantom cost line.

### `SessionAccountingSnapshot` — the conversation-shape view

Provider tallies say *what was charged*. They do not say *why*. That is the
job of `SessionAccountingSnapshot`, built from the live conversation buffer:

```rust
// crates/squeezy-agent/src/lib.rs:534-550
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionAccountingSnapshot {
    pub session_id: Option<String>,
    pub provider: &'static str,
    pub model: String,
    pub mode: SessionMode,
    pub store_responses: bool,
    pub previous_response_id: Option<String>,
    pub cost: CostSnapshot,
    pub metrics: SessionMetrics,
    pub redactions: u64,
    pub transcript: TranscriptShape,
    pub conversation: ConversationShape,
    pub attachments: AttachmentShape,
    pub transmitted_request: RequestTokenEstimate,
    pub full_history_request: RequestTokenEstimate,
}
```

`ConversationShape` (`crates/squeezy-agent/src/lib.rs:509-522`) is the
byte-level breakdown by item kind. It carries item-count fields
(`user_text`, `assistant_text`, `function_calls`, `function_outputs`,
`reasoning_items`, `image_items`) alongside four byte counters:
`text_bytes`, `tool_output_bytes`, `reasoning_bytes`, `image_bytes`.

It is filled by one linear pass over the conversation
(`crates/squeezy-agent/src/lib.rs:11611-11644`). Each match arm increments an
item-count field and adds the wire length to one of four byte counters:

```rust
// crates/squeezy-agent/src/lib.rs:11626-11641 (excerpt)
LlmInputItem::FunctionCall { arguments, .. } => {
    shape.function_calls += 1;
    shape.text_bytes += arguments.to_string().len();
}
LlmInputItem::FunctionCallOutput { output, .. } => {
    shape.function_outputs += 1;
    shape.tool_output_bytes += output.len();
}
LlmInputItem::Reasoning(payload) => {
    shape.reasoning_items += 1;
    shape.reasoning_bytes += payload.display_text().len();
}
LlmInputItem::Image { bytes, .. } => {
    shape.image_items += 1;
    shape.image_bytes += bytes.len();
}
```

`text_bytes` aggregates user text, assistant text, *and* function-call
arguments — all the prose bucket — while `tool_output_bytes`,
`reasoning_bytes`, and `image_bytes` get their own counters because those
are the cuts that the verbosity knobs target.

The `RequestTokenEstimate` carries the budget math:

```rust
// crates/squeezy-llm/src/registry.rs:107-120
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestTokenEstimate {
    pub input_tokens: u64,
    pub context_window_tokens: Option<u64>,
    pub effective_context_window_tokens: Option<u64>,
    pub headroom_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub input_budget_tokens: Option<u64>,
    pub remaining_input_tokens: Option<u64>,
    /// Hundredths of one percent. `10_000` means 100.00%.
    pub used_input_percent_x100: Option<u32>,
    pub tokenizer: TokenizerKind,
    pub estimated: bool,
}
```

`used_input_percent_x100` is computed against `input_budget_tokens`, itself
`effective_context_window_tokens` minus the reserved `max_output_tokens` minus
a fixed `BASELINE_TOKENS = 12_000` headroom
(`crates/squeezy-llm/src/registry.rs:299-322`). The `_x100` suffix preserves
two decimal places without a float in the struct — 47.83% of budget is
`4783`. Two estimates are built per snapshot: `transmitted_request` (what
went over the wire after any provider-side replay of stored responses) and
`full_history_request` (what the local buffer would total if re-sent fresh).
The gap between them tells the user whether `store_responses=true` is
paying off.

### `/context` output

`format_context_command` (`crates/squeezy-tui/src/commands.rs:135-287`)
assembles the user-facing report. The header takes
`snapshot.transmitted_request.input_tokens` as `consumed` and prints
`consumed`, `remaining = window - consumed`, and both percentages against
`context_window_tokens` (`crates/squeezy-tui/src/commands.rs:140-152`).
If the model registry has no window entry, it prints only `consumed` with a
"context window unknown" caveat.

The per-source section converts each byte counter from `ConversationShape`
into approximate tokens at four bytes per token, then derives the
system/schema share by subtraction so it reconciles to `consumed`:

```rust
// crates/squeezy-tui/src/commands.rs:174-214 (excerpt)
let approx = |bytes: usize| bytes.div_ceil(4);
let user_tokens = approx(snapshot.conversation.text_bytes);
let tool_tokens = approx(snapshot.conversation.tool_output_bytes);
let reasoning_tokens = approx(snapshot.conversation.reasoning_bytes);
let image_tokens = approx(snapshot.conversation.image_bytes);
let attachment_tokens = approx(snapshot.attachments.stored_bytes);
let accounted = user_tokens + tool_tokens + reasoning_tokens + image_tokens + attachment_tokens;
let system_estimate = consumed.saturating_sub(accounted as u64);
```

The system/framing line is `consumed - accounted`, so the breakdown always
reconciles. The Session block restates the provider tally — turns, input,
output, reasoning, cached input, cache-write input — and the Request
estimates block prints `transmitted_request` and `full_history_request` side
by side. The redaction count comes from `AttachmentShape::redactions`.

## Worked example

A user is twenty turns into a debug session on a 200K-window Claude model.
They run `/context`. The session has:

- `ConversationShape.text_bytes = 88_400` (user + assistant + function-call
  args)
- `ConversationShape.tool_output_bytes = 192_000` (a few `grep` runs and
  several `read_file` outputs)
- `ConversationShape.reasoning_bytes = 0`, `image_bytes = 0`,
  `AttachmentShape.stored_bytes = 0`
- `transmitted_request.input_tokens = 94_120`
- `transmitted_request.context_window_tokens = Some(200_000)`

The header prints `consumed: 94120 tokens (47.1% of 200000 window)`. The
per-source block runs `div_ceil(4)` over each byte counter:

- user + assistant text: `~22_100 tokens`
- tool call outputs: `~48_000 tokens`
- system prompt + framing: `~24_020 tokens` (the remainder)

Tool outputs are 51% of the consumed budget. The user runs
`/compact`. On the next turn, stale raw tool outputs are replaced by compact
summaries or receipt stubs, `tool_output_bytes` drops to roughly 64_000, and the
next `/context` shows `~62_000 tokens (~31% of window)`. The
`SessionAccountingSnapshot` made the decision obvious — no guessing whether
the system prompt, the conversation, or the tools was the driver. The
compaction trigger reads the same `transmitted_request.used_input_percent_x100`
the user just saw, so user-visible telemetry and automated triggers fire off
identical numbers.

## Edge cases and limits

**OpenAI's missing `cache_write_input_tokens`.** The provider does not
distinguish a first write into the prefix cache from an uncached miss, so the
field stays `None`. The estimator treats `None` as zero via
`cost.cache_write_input_tokens.unwrap_or(0)` and the first-write tokens fall
under the standard input rate. This slightly understates the first cached
turn and matches reality on every subsequent one. Same shape for Google.

**Mid-stream usage emission timing.** Anthropic emits `usage` on the final
`message_delta` and again on `message_stop`. Squeezy merges every `usage` it
sees (`crates/squeezy-llm/src/anthropic.rs:1002`) and emits `Completed` with
the consolidated `cost()` only at `message_stop`. A stream error mid-message
leaves the partial `usage` in state but no `Completed` event — the turn is
recorded as failed and the partial cost does not pollute the running tally.
Google emits running totals on every chunk and Squeezy overwrites, so the
final value wins.

**Attached but stripped media.** `AttachmentShape::stored_bytes` aggregates
*all* stored bytes regardless of status (`attachment_shape`,
`crates/squeezy-agent/src/lib.rs:11647-11662`). An attachment in `Removed`
status still occupies the local store but is not sent on the wire, so the
`/context` "attached context" line overcounts after a session has dropped
attachments. The provider tally and `transmitted_request.input_tokens` are
unaffected — they reflect the wire payload, not the store.

**Image-token estimation.** The local estimator approximates every image at a
flat 1024 tokens plus the base64 wire size divided by `bytes_per_token`
(`crates/squeezy-llm/src/registry.rs:478-482`). Anthropic charges roughly one
tile per 750 image pixels; OpenAI charges 85 base plus 170 per high-detail
tile; Gemini charges 258 per image. The 1024-token floor reserves real
budget headroom for an image without bloating text-heavy turns; the
provider's true charge arrives in the next `CostSnapshot.input_tokens` after
the turn completes.

**Calibration.** The bytes-per-token ratio is per provider, and the agent
keeps a learned `TokenCalibration` (`crates/squeezy-agent/src/lib.rs:1941`)
that reconciles each turn's local estimate against the provider's actual
`input_tokens`. Subsequent `RequestTokenEstimate` values use the calibrated
ratio, so the prediction tightens turn over turn even before the model
responds.

## Cost intuition

Token accounting is the substrate every other saving mechanism stands on.
Compaction needs `used_input_percent_x100` to know when to fire. The
cache-read line in `/context` is what tells the user prompt caching is
actually working. `/tool-verbosity` and `/verbosity` are only useful because
the per-source breakdown points the user at the bucket that is large.
Session-mode toggles are justified by the `estimated_usd_micros` running
total.

Provider tallies alone would not be enough — they say the bill was high but
not what to cut. Conversation-shape counters alone would miss the cache
discount and the reasoning surcharge. `SessionAccountingSnapshot` holds both
views in one struct so the `/context` formatter can put the provider charge
and the local breakdown that explains it on the same screen. Cutting tokens
without measuring them is gambling; Squeezy's design measures first, exposes
the measurement, and points every saving mechanism at the same numbers.
