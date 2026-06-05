# Provider-Side Prompt Caching

## Motivation

A coding agent re-sends the same large prefix on every turn. Three components
dominate:

1. **System prompt** — agent identity, tool-use rules, environment context.
   In Squeezy this runs from a few thousand to tens of thousands of tokens
   once skills and project context are folded in.
2. **Tool definitions** — every JSON schema for every advertised tool. A
   baseline session advertises ~12 tools; MCP servers push that to 30+. Each
   schema is typically 100-400 tokens.
3. **Message history** — every prior user message, assistant reply, tool
   call, and tool result. After a handful of file reads, this dominates the
   request: a single `Read` of a 1 KLOC file adds ~5 K tokens of tool-result
   content replayed verbatim on every subsequent turn.

Without caching the provider re-tokenizes the full prefix on every turn at
the base input rate. With caching the provider serves the matching prefix
from a hot KV cache and bills the matched share at a fraction of the input
price — typically 10% on Anthropic, 50% on OpenAI. Squeezy's job is to
attach *breakpoint markers* so the provider knows where the cacheable
prefix ends, and to keep the bytes *before* the marker byte-stable across
turns so the provider's prefix-hash lookup keeps hitting.

## Mechanism

### Central policy: `cache_policy.rs`

The Anthropic-family adapters route their inline breakpoint-placement
decisions through `crates/squeezy-llm/src/cache_policy.rs`: native
Anthropic JSON, OpenAI-compatible Anthropic routes, and Bedrock's typed
`CachePoint` blocks. OpenAI Responses uses server-side hash routing
(`prompt_cache_key` / retention fields) rather than inline breakpoints,
and Google currently has no client-created cache resource in Squeezy. The
policy module centralizes the *where to mark* decision for providers that
do accept markers; other providers only contribute usage/accounting data.

The retention enum at `cache_policy.rs:42-57` is the public knob a caller
flips: `None` (no caching, default), `Short` (provider default window —
Anthropic 5m, OpenAI in-memory), or `Long` (extended TTL). Three bands map
to three provider-specific knobs:

- Anthropic: `Long` -> `cache_control: { type: "ephemeral", ttl: "1h" }`;
  `Short` -> marker without `ttl` (5m default); `None` -> no marker.
- OpenAI Responses: `Long` -> top-level `prompt_cache_retention: "24h"`;
  `Short` / `None` -> field omitted.
- Compatible (Anthropic-via-aggregator): mirrors Anthropic.

A request carries this on `CacheSpec { key: Option<String>, retention:
CacheRetention }` at `cache_policy.rs:70-76`. The legacy
`LlmRequest::cache_key: Option<String>` is lifted into a `CacheSpec` at the
provider boundary via `From<Option<String>>` (`cache_policy.rs:78-93`),
which yields `Short` retention for any non-`None` legacy key, preserving
the pre-retention-enum 5m default for old callers without code changes.

#### Gating: `should_apply_caching`

Before any adapter places a marker it consults `should_apply_caching` at
`cache_policy.rs:135-139`:

```rust
// crates/squeezy-llm/src/cache_policy.rs:135-139
pub(crate) fn should_apply_caching(provider: &str, request: &LlmRequest) -> bool {
    request.effective_cache_spec().retention != CacheRetention::None
        && capabilities_for(provider, &request.model)
            .is_some_and(|capabilities| capabilities.prompt_caching)
}
```

Two gates: the caller asked for caching (retention is `Short` or `Long`) and
the static model registry reports `prompt_caching` capability for that
`(provider, model)` pair. The registry gate stops Squeezy from sending a
`cache_control` block to a model that would 400 on it.

#### The marker shape

The shared ephemeral marker literal lives at `cache_policy.rs:147-153`:

```rust
// crates/squeezy-llm/src/cache_policy.rs:147-153
pub(crate) fn ephemeral_marker(retention: CacheRetention) -> Value {
    if retention == CacheRetention::Long {
        json!({ "type": "ephemeral", "ttl": "1h" })
    } else {
        json!({ "type": "ephemeral" })
    }
}
```

Anthropic Messages, OpenAI-compatible aggregator routes pointed at Anthropic
models, and the Bedrock typed `CachePoint` block all rely on this single
function.

#### Marker placement: stable breakpoints plus a tail anchor

`CachePolicy::AUTO` at `cache_policy.rs:117-124` is the only policy any
adapter currently uses; it enables tools, system, and a
`MessageStrategy::LatestUserMessage` choice. Native Anthropic can use up to
four `cache_control` breakpoints per request: the three structural markers
below plus a "stable-tail anchor" behind the moving latest user block when
the marker budget has room. Semantics: cache everything up to and including
the marker.

1. **System tail.** `system_array_with_marker` (`cache_policy.rs:206-212`)
   wraps the system string in the array form Anthropic requires and pins the
   marker onto the trailing text block.
2. **Last stable tool.** `mark_last_stable_tool`
   (`cache_policy.rs:241-255`) walks the tools array and picks the
   breakpoint index via `last_stable_tool_index`
   (`cache_policy.rs:175-190`). The index skips `mcp__`-prefixed names so a
   dynamic tool registry refresh doesn't invalidate the cached tool prefix.
   See "Edge cases".
3. **Last user block.** `mark_last_user_block` (`cache_policy.rs:216-234`)
   walks messages back-to-front, finds the most recent user message, and
   pins the marker onto its trailing content block.
4. **Stable-tail anchor.** `mark_stable_anchor_block` walks back from the
   message tail and marks an older user block so the just-settled tail can
   be cache-read on the next turn instead of repeatedly cache-written. This
   marker is optional and is the first one dropped if future marker users
   consume the four-marker Anthropic budget.

There is no separate "last assistant" marker; the next turn's history
naturally contains everything up to the previous assistant reply, and the
prefix-hash lookup walks forward until new content begins.

### Anthropic native (`anthropic.rs`)

`AnthropicProvider::request_body` at `anthropic.rs:144-242` opens by deriving
the retention from the request and the gate:

```rust
// crates/squeezy-llm/src/anthropic.rs:144-152
pub(crate) fn request_body(request: &LlmRequest, auth: AnthropicAuthScheme) -> Value {
    let policy = CachePolicy::AUTO;
    let prompt_caching = should_apply_caching("anthropic", request);
    let retention = if prompt_caching {
        request.effective_cache_spec().retention
    } else {
        CacheRetention::None
    };
```

Three placements follow:

- **System** — `anthropic_system` (`anthropic.rs:296-332`) wraps the
  instructions in the array form and appends `cache_control` to the
  trailing text block. The OAuth (Claude Pro/Max) path additionally
  prepends a fixed Anthropic-required identity string as a separate text
  block *before* the cacheable user instructions.
- **Tools** — at `anthropic.rs:224-240` the adapter calls
  `json_markers::mark_last_stable_tool` once the tool array is materialized.
- **Last user block** — `anthropic_messages` at `anthropic.rs:334-432`
  builds the messages array and finishes with
  `json_markers::mark_last_user_block` (`anthropic.rs:424-430`).

The response side reads `cache_read_input_tokens` and
`cache_creation_input_tokens` from the Anthropic SSE `usage` block at
`anthropic.rs:1044-1065`. The stream state normalizes the totals at
`anthropic.rs:734-756`:

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

Anthropic's wire ships `usage.input_tokens` as the *uncached delta only*;
Squeezy folds the cache counters back in so `CostSnapshot.input_tokens`
reflects the full prompt the model saw. The cached share lives in
`cached_input_tokens` / `cache_write_input_tokens` separately for the cost
engine to bill at the right rate.

### OpenAI Responses API (`openai.rs`)

OpenAI's prompt caching is hash-based on the server: there are no inline
breakpoint markers in the body. Two knobs (`openai.rs:170-181`):

```rust
// crates/squeezy-llm/src/openai.rs:170-181
let cache_spec = request.effective_cache_spec();
if let Some(key) = cache_spec.key.as_deref() {
    body["prompt_cache_key"] = json!(clamp_prompt_cache_key(key));
}
if cache_spec.retention == crate::CacheRetention::Long {
    body["prompt_cache_retention"] = json!("24h");
}
```

`prompt_cache_key` is a stable identifier that OpenAI uses to *route* the
session to the same backend node that warmed the prefix. Without it a
multi-turn session can land on a cold node mid-conversation and re-pay full
uncached input on the next turn even though the prefix would otherwise hit.
The 64-codepoint clamp at
`crates/squeezy-llm/src/openai_prompt_cache.rs:28-36` exists because OpenAI
silently drops longer keys server-side — the request still succeeds, the
cache simply never warms.

Squeezy also pins routing affinity headers at `openai.rs:264-269`:
`affinity_headers` emits `session_id` and `x-client-request-id` both set to
the cache key. The body field is clamped to OpenAI's 64-codepoint limit;
the headers carry up to 256 bytes of the same key, clamped on a UTF-8
boundary to avoid oversized request headers while still giving the load
balancer a larger affinity space to bin on.

### Bedrock typed `CachePoint` (`bedrock.rs`)

Bedrock's Converse API uses typed AWS-SDK content blocks rather than inline
JSON. `cache_point_block` (`bedrock.rs:455-462`) builds a single
`CachePointBlock` with `CachePointType::Default`. Three placements append it:

- **System** — `system_blocks` (`bedrock.rs:464-476`) pushes a
  `SystemContentBlock::CachePoint` after the text block.
- **Tools** — `tool_configuration` inserts one `Tool::CachePoint` after the
  last non-`mcp__` tool. If every advertised tool is dynamic, the tool-level
  cache point is omitted instead of anchoring on a volatile registry tail.
- **Last user message** — `append_cache_point_to_last_user`
  (`bedrock.rs:583-604`) finds the last user message via `rposition`, copies
  its content, appends a `ContentBlock::CachePoint`, and rebuilds the
  immutable `Message` because the SDK requires reconstruction.

The Bedrock stream state at `bedrock.rs:281-301` mirrors the Anthropic
normalization: `usage.inputTokens` arrives as the uncached delta and gets
folded back into the total `input_tokens` while the cached share lives in
`cached_input_tokens` / `cache_write_input_tokens`.

The Bedrock adapter now follows the same stable-tool intent as the JSON
helpers, but expressed with the typed SDK shape. It cannot place
`cache_control` on an individual tool object; it inserts a separate
`Tool::CachePoint` block immediately after the selected stable tool.

### Google `cachedContent` (`google.rs`)

Google's Gemini caching surface is different from the other four: there is
no client-emitted breakpoint marker and no `cache_control` block. Google's
implementation is *implicit prefix caching* on the server side, controlled
through a separate `cachedContent` resource that has to be created with a
distinct HTTP call. Squeezy does not currently materialize that resource.

The Gemini request body at `google.rs:54-103` carries no caching fields:

```rust
// crates/squeezy-llm/src/google.rs:54-72
pub(crate) fn request_body(request: &LlmRequest) -> Value {
    let normalized_input = crate::normalize_tool_ids_for_replay(&request.input);
    let mut body = json!({
        "systemInstruction": {
            "parts": [{"text": request.instructions}]
        },
        "contents": google_contents(&normalized_input),
        "generationConfig": {},
    });
```

What Squeezy *does* do is read the implicit-cache outcome from
`usageMetadata.cachedContentTokenCount` at `google.rs:368`:

```rust
// crates/squeezy-llm/src/google.rs:365-370
if let Some(usage) = value.get("usageMetadata") {
    cost.input_tokens = usage.get("promptTokenCount").and_then(Value::as_u64);
    cost.cached_input_tokens = usage.get("cachedContentTokenCount").and_then(Value::as_u64);
    let visible = usage.get("candidatesTokenCount").and_then(Value::as_u64);
    let thoughts = usage.get("thoughtsTokenCount").and_then(Value::as_u64);
    cost.output_tokens = match (visible, thoughts) {
        (None, None) => None,
        (visible, thoughts) => Some(visible.unwrap_or(0) + thoughts.unwrap_or(0)),
    };
    cost.reasoning_output_tokens = thoughts;
}
```

This is enough to *bill* Google's implicit prefix-cache hits correctly,
because Google handles the prefix-match server-side as long as the
`contents` slice is byte-stable. Squeezy's job on Google is therefore
narrower than on the other providers: build a stable `contents` array (the
`normalize_tool_ids_for_replay` call upstream handles cross-provider
id-shape stability) and read the resulting `cachedContentTokenCount` for
billing. The cost-engine downstream applies Google's reduced cache rate to
this counter.

### OpenAI-compatible aggregators (`compatible.rs`)

Aggregators like OpenRouter, Vercel AI Gateway, and PortKey forward bodies
to upstream providers. When the destination model namespace is
`anthropic/...`, the upstream is Anthropic and the aggregator forwards
Anthropic-style `cache_control` markers verbatim. When it's `openai/...`,
the upstream honors `prompt_cache_key` and, where accepted,
`prompt_cache_retention`. Squeezy emits the OpenAI affinity key whenever one
is configured, but suppresses retention on presets known to reject unknown
fields such as Mistral.

The flavor decision is table-driven via `COMPAT_TABLE`
(`compatible.rs:374-403`). Each `CompatEntry` carries `model_prefix`,
`flavor`, and a `supports_cache_control: bool`. The currently registered
entries: `anthropic/` (cache-control on), `openai/`, `google/`, `xai/`
(cache-control off). `compat_entry`+`supports_anthropic_caching`
(`compatible.rs:421-437`) read the flag and the request-body assembly at
`compatible.rs:149-153` derives:

```rust
// crates/squeezy-llm/src/compatible.rs:149-153
let cache_spec = request.effective_cache_spec();
let cache_retention = cache_spec.retention;
let anthropic_caching =
    cache_retention != CacheRetention::None && supports_anthropic_caching(&request.model);
let cache_control = anthropic_caching.then(|| ephemeral_marker(cache_retention));
```

The three Anthropic-style markers (system, last user block, last stable
tool — same helpers as the native Anthropic adapter, see
`compatible.rs:178-205` and `compatible.rs:262-281`) each gate on
`anthropic_caching`. The OpenAI-style `prompt_cache_key` is emitted whenever
the spec sets it; long retention is emitted only for presets that accept it:

```rust
// crates/squeezy-llm/src/compatible.rs:225-246 (abbreviated)
if let Some(key) = cache_spec.key.as_deref() {
    body["prompt_cache_key"] = json!(compatible_prompt_cache_key(key));
}
if cache_retention == CacheRetention::Long
    && !preset_rejects_prompt_cache_retention(preset)
{
    body["prompt_cache_retention"] = json!("24h");
}
```

This way an aggregator that forwards to OpenAI-hosted upstream picks up the
right field, an Anthropic-hosted upstream picks up the markers, and other
upstreams ignore the unknown fields unless a preset has proven stricter. Long
compatible keys are hashed before the final OpenAI-style clamp so distinct
workspace/session keys do not collapse to the same 64-codepoint prefix.

## Worked example

Consider an Anthropic turn after four prior turns of editing a Rust file.
`AnthropicProvider::request_body` produces a JSON body whose top-level keys
are `model`, `system`, `messages`, `max_tokens`, `stream`, `tools`. Up to
four markers exist in this request, at fixed positions (abbreviated):

```json
{
  "model": "claude-opus-4-7",
  "system": [
    { "type": "text", "text": "<system prompt>",
      "cache_control": { "type": "ephemeral" } }
  ],
  "tools": [
    { "name": "Read" }, { "name": "Write" }, { "name": "Edit" },
    { "name": "Bash" },
    { "name": "Grep", "cache_control": { "type": "ephemeral" } },
    { "name": "mcp__github__list_issues" },
    { "name": "mcp__github__open_pr" }
  ],
  "messages": [
    { "role": "user",      "content": [ { "type": "text", "text": "fix bug" } ] },
    { "role": "assistant", "content": [ { "type": "tool_use", "id": "toolu_01", "name": "Read" } ] },
    { "role": "user",      "content": [ { "type": "tool_result", "tool_use_id": "toolu_01", "content": "<5K file>" } ] },
    { "role": "assistant", "content": [ { "type": "tool_use", "id": "toolu_02", "name": "Edit" } ] },
    { "role": "user",      "content": [ { "type": "tool_result", "tool_use_id": "toolu_02", "content": "ok" } ] },
    { "role": "assistant", "content": [ { "type": "text", "text": "Fixed." } ] },
    { "role": "user", "content": [
        { "type": "text", "text": "now write a test",
          "cache_control": { "type": "ephemeral" } } ] }
  ]
}
```

The first three markers are structural breakpoints:

1. **System tail** — `anthropic_system` (`anthropic.rs:296-332`) attaches it
   to the single system text block.
2. **Last stable tool** — pinned on `Grep` rather than the trailing
   `mcp__github__open_pr` because `last_stable_tool_index`
   (`cache_policy.rs:175-190`) walks back skipping the `mcp__` prefix.
3. **Last user block** — `mark_last_user_block` (`cache_policy.rs:216-234`)
   pins it onto the most recent user message.

When Anthropic's four-marker budget has room, Squeezy also adds a
stable-tail anchor behind the newest user block. That extra marker keeps the
settled message tail cache-readable on the next turn instead of repeatedly
paying the cache-write premium.

Byte-stability across turns:

- **System** is a fixed string per session; no per-turn timestamps or
  random ids are interpolated.
- **Tools** are deterministically ordered; MCP tools live at the tail, so
  the cached prefix ends *before* any dynamic entry. An MCP `tools/list`
  refresh that mutates the tail does not invalidate the prefix.
- **Messages** before the last-user breakpoint are immutable history. Each
  turn appends to the array tail.

On turn N+1 the new marker moves to the new user message; the previous
breakpoint becomes part of the cacheable prefix. Anthropic's server hashes
forward across the JSON until new content begins and serves the matched
prefix from KV cache.

## Edge cases & limits

### MCP tools are excluded from the breakpoint index

`cache_policy.rs:155-190` reserves the `mcp__` name prefix for dynamically
advertised MCP tools and skips them when picking the breakpoint:

```rust
// crates/squeezy-llm/src/cache_policy.rs:155-190
pub(crate) const DYNAMIC_TOOL_NAME_PREFIX: &str = "mcp__";

pub(crate) fn last_stable_tool_index<'a, I>(names: I) -> Option<usize>
where
    I: IntoIterator<Item = &'a str>,
    I::IntoIter: DoubleEndedIterator + ExactSizeIterator,
{
    let iter = names.into_iter();
    let len = iter.len();
    if len == 0 {
        return None;
    }
    let stable = iter
        .enumerate()
        .rev()
        .find_map(|(idx, name)| (!name.starts_with(DYNAMIC_TOOL_NAME_PREFIX)).then_some(idx));
    Some(stable.unwrap_or(len - 1))
}
```

Reason: an MCP `tools/list` refresh can re-order, add, or drop dynamic tools
between turns. Anything *after* the cache breakpoint can change without
invalidating the cached prefix. By placing the breakpoint on the last
*stable* (non-MCP) tool, an MCP refresh that mutates the tail of the tools
array leaves the cached prefix intact. The JSON helper falls back to the
literal last index when every advertised tool is dynamic; Bedrock's typed
tool configuration instead omits the tool-level cache point in that case.

The native Anthropic, the OpenAI-compatible Anthropic-via-aggregator path,
and the Bedrock path all share this invariant: dynamic tools live at the
tail, while stable first-party tools remain before the cache boundary.

### How `Long` vs `Short` retention is chosen

The retention enum is set on the request via `request.cache.retention`, and
defaults to `None` if the caller doesn't touch it. The legacy `cache_key`
field defaults to `Short` via the `From<Option<String>>` impl at
`cache_policy.rs:78-93`. Callers pick `Long` explicitly when a session is
expected to span hours of human typing (an IDE-attached chat that stays open
across lunch breaks); `Short` is the right default when the agent is
running fully autonomously inside a 5-minute window.

The cost trade is concrete: cache *writes* on Anthropic are billed at ~25%
above the base input rate. Writing a `Long` (1h TTL) prefix costs more per
write than writing the default 5m prefix. If the prefix won't be re-read
within the hour, `Long` is a loss. Squeezy's policy module exposes the
choice but does not auto-tune it.

### What invalidates the cache

The cache key is the *prefix hash up to and including the marker*. Any byte
change before the marker invalidates the cached prefix. Common
invalidators:

- Editing the system prompt mid-session (changing the active skill,
  swapping personas, toggling the safety preamble).
- Reordering the tools array. Squeezy's registry orders deterministically
  and pushes MCP tools to the tail, but a caller that builds a custom
  `tools` slice in a non-deterministic order will miss cache.
- The first non-deterministic byte. A timestamp interpolated into the
  system prompt, a random UUID stamped into the agent identity, or a
  per-turn skill-version line all defeat prefix matching.

### 1h vs 24h TTL choice

Anthropic exposes a 5m (default) and 1h (`ttl: "1h"`) cache window. OpenAI's
Responses API exposes a 24h window via `prompt_cache_retention`. Squeezy
compresses both into `CacheRetention::Long` and emits the provider-side
directive — `ttl: "1h"` on Anthropic (`cache_policy.rs:147-153`),
`prompt_cache_retention: "24h"` on OpenAI (`openai.rs:179-181`). A caller
asking for `Long` doesn't have to know per-provider TTL constants. The
compatible adapter mirrors this and emits both on the same request so an
aggregator can forward the relevant one to the right upstream
(`compatible.rs:238-246`).

### No cache-write tracking on OpenAI

The Anthropic and Bedrock stream-state structs carry both
`cache_read_input_tokens` and `cache_creation_input_tokens` (Bedrock
`cache_write_input_tokens`); see `anthropic.rs:716-717` and
`bedrock.rs:271-272`. The OpenAI Responses API does *not* break out
cache-write tokens in `usage` — it ships only `cached_tokens` (reads). This
means the cost engine cannot apply OpenAI's cache-write premium per-turn
because the count is not provider-reported; on OpenAI, the "cost of the
first turn that fills the cache" gets billed as plain input. Anthropic and
Bedrock both report the write counter so the engine can apply the
~25% premium accurately.

## Cost intuition

Take a 20-turn coding session on an Anthropic model with a 30 K-token stable
prefix (8 K system + 6 K tools + 16 K accumulated history) and 2 K of new
content per turn. Using Anthropic's cache-read/cache-write billing shape as
normalized units, cache-read is 10% of base and cache-write is 125%.

- **Uncached**: 20 * 32K = 640 K input tokens at base. Normalize to 640
  cost units.
- **Cached (Short)**, turn 1 (write): `(30K * 1.25 + 2K * 1) / 32K = 1.23`x
  base. Turns 2-20 (read): `(30K * 0.10 + 2K * 1) / 32K = 0.156`x base.
  Total: `1 * 32K * 1.23 + 19 * 32K * 0.156 = 39.5 + 94.8 = 134.3` units.

That's a ~79% reduction on the input share. Output tokens are unaffected
(the cache is input-side only); coding sessions are input-heavy because
history dominates, which is what makes prompt caching the single
highest-leverage cost lever in Squeezy's stack.

The math flips for very short sessions: a 2-turn session is
`(32K * 1.23 + 32K * 0.156) / (2 * 32K) = 0.69`x base — still cheaper, but
only ~30% off. Below 2 turns the write premium isn't recovered, which is
why `CacheRetention::None` is the default and `should_apply_caching`
(`cache_policy.rs:135-139`) gates on the caller's explicit retention.

For OpenAI the per-hit math is weaker (cache reads at ~50% of base, not
10%), so the affinity-routing investment via `prompt_cache_key` and the
`session_id` / `x-client-request-id` headers (`openai.rs:264-269`) — what
keeps a session pinned to the warmed backend node — is the dominant lever
rather than marker-placement strategy.
