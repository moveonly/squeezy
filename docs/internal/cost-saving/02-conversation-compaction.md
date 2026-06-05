# Conversation Compaction

## Motivation

A chat-style coding agent ships the entire conversation back to the model on
every turn. The provider charges per input token, so an N-turn session's total
input bill grows as the sum 1 + 2 + ... + N, which is O(N^2). A 30-turn debug
session where each turn appends ~5 KB of new context (user message, assistant
text, a couple of `read_file` outputs, a `grep` result) ends with a ~150 KB
final-turn request, and the cumulative input across all turns is ~2.25 MB even
though the user only typed a few sentences.

That growth is doubly punishing. First, tool outputs dominate: one
`read_file` against a 30 KB source file dwarfs every user message in the
session combined. Second, providers reject requests that exceed
`model_context_window` (200K tokens on Claude, 128K on most OpenAI models),
so without compaction the agent simply stops working past a certain depth.

Squeezy's compaction subsystem swaps the older portion of the conversation
for a synthetic summary item before the next request goes out. The trade-off
is explicit: the model loses verbatim recall of older tool outputs, gaining
a structured prose summary plus carry-forward file lineage. Pinned items and
the most recent N turns stay verbatim, so the model never has to guess at
the immediate working state.

## Mechanism

### Micro tier plus full-compaction triggers

Squeezy now has two compaction tiers. The full tier still replaces the older
conversation prefix with a summary head, but the mid-turn path first tries a
cheaper micro-compaction pass. `maybe_micro_compact_mid_turn` rewrites older
compactable `FunctionCallOutput` bodies in place once the conversation reaches
`micro_compaction_threshold_percent` of `model_context_window` (default 60%).
It keeps the newest compactable outputs verbatim, preserves every
`FunctionCall`/`FunctionCallOutput` pairing by `call_id`, and can avoid the
full summary rewrite entirely. If the conversation is still over the full
threshold, `maybe_compact_mid_turn` runs next.

Two entry points decide when to run the full tier. The post-turn
`maybe_compact_conversation` runs after every assistant turn finishes; the
mid-turn `maybe_compact_mid_turn` runs before the next sample when the
provider-reported token usage crosses the configured full threshold.

```rust
// crates/squeezy-agent/src/context_compaction.rs:143-203
pub(crate) fn maybe_compact_mid_turn(
    conversation: &mut Vec<LlmInputItem>,
    state: &mut ContextCompactionState,
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
    last_total_tokens: Option<u64>,
) -> Option<ContextCompactionReport> {
    if !config.context_compaction.enabled_mid_turn {
        return None;
    }
    let window = config.context_compaction.model_context_window?;
    if window == 0 {
        return None;
    }
    let threshold = window
        .saturating_mul(config.context_compaction.threshold_percent.min(100) as u64)
        .saturating_div(100);
    let observed =
        last_total_tokens.unwrap_or_else(|| estimate_context(conversation).estimated_tokens);
    if observed < threshold {
        return None;
    }
    compact_conversation(
        conversation,
        state,
        attachments,
        store,
        config,
        ContextCompactionTrigger::Auto,
        true,
    )
}

pub(crate) fn maybe_compact_conversation(
    conversation: &mut Vec<LlmInputItem>,
    state: &mut ContextCompactionState,
    attachments: &[ContextAttachment],
    store: Option<&SqueezyStore>,
    config: &AppConfig,
    trigger: ContextCompactionTrigger,
) -> Option<ContextCompactionReport> {
    if !config.context_compaction.enabled {
        return None;
    }
    let estimate = estimate_context(conversation);
    if estimate.items < config.context_compaction.min_items
        || estimate.estimated_tokens < config.context_compaction.estimated_tokens
    {
        return None;
    }
    compact_conversation(
        conversation,
        state,
        attachments,
        store,
        config,
        trigger,
        false,
    )
}
```

The two paths read different thresholds:

- Post-turn (`context_compaction.rs:189`) gates on two AND-ed conditions:
  the conversation must have at least `min_items` items *and* its estimated
  token count must exceed `estimated_tokens`. Defaults are 16 items and
  60,000 tokens (`squeezy-core/src/lib.rs:290-293`).
- Mid-turn (`context_compaction.rs:158`) gates on a percentage of the
  configured `model_context_window`. The default `threshold_percent` is 80
  (`squeezy-core/src/lib.rs:298`). The trigger uses the provider's reported
  `last_total_tokens` when available — the live usage figure from the
  streaming response — and falls back to the byte-derived estimator when
  the provider has not sent a usage event yet.

Both ultimately delegate to `compact_conversation` (`context_compaction.rs:205`),
which holds the actual splitting logic.

### Splitting older from recent

```rust
// crates/squeezy-agent/src/context_compaction.rs:214-236
let before = estimate_context(conversation);
let keep = config.context_compaction.recent_items.max(1);
if !force && before.items <= keep {
    return None;
}
let initial_split = conversation.len().saturating_sub(keep);
if initial_split == 0 {
    return None;
}
// ... snap_compaction_split absorbs any leading orphan FunctionCallOutput ...
let split = snap_compaction_split(conversation, initial_split);
if split == 0 || split >= conversation.len() {
    return None;
}

let older = conversation[..split].to_vec();
let recent = conversation[split..].to_vec();
```

`recent_items` defaults to 10. That means at most ten items at the tail of
the conversation (each item being one of `UserText`, `AssistantText`,
`FunctionCall`, `FunctionCallOutput`, `Reasoning`, or `Image`) are preserved
verbatim. The split point is then snapped forward by
`snap_compaction_split` (`context_compaction.rs:350-371`) so a
`FunctionCallOutput` whose declaring `FunctionCall` is in the older slice
gets absorbed into older — otherwise the OpenAI Responses API rejects a
payload that references a `call_id` that isn't also present as a
`function_call`. `drop_orphan_function_call_outputs` (`:377`) handles
parallel-call shapes (`[FC(A), FC(B), FCO(A), FCO(B)]`) where snap can't
fix the boundary alone.

### Media stripping before the summarizer

Before either summarizer sees the older slice, base64 image and PDF data
URIs embedded in tool outputs are replaced with `[image]` / `[document]`
markers:

```rust
// crates/squeezy-agent/src/context_compaction.rs:248-256
let older_for_summary = strip_media_for_compaction(&older);
let summary = build_compaction_summary(
    generation,
    state,
    &older_for_summary,
    attachments,
    store,
    config,
);
```

The strip operates on both `FunctionCallOutput.output` strings and
`content_parts`. Text parts are scanned for `data:<mime>;base64,` prefixes,
then advanced until the first non-base64 byte. Image content parts are
replaced by short placeholders so a screenshot cannot smuggle raw bytes into
the compaction prompt. The `dropped` slice persisted for undo is built from
the *original* `older`, not the stripped copy, so an undo restores the
verbatim bytes the model saw at the time. Skipping short outputs avoids
per-token overhead on the common short-output case.

### Extractive summary structure

`build_compaction_summary` (`context_compaction.rs:895-992`) is the
deterministic backbone. It composes a multi-section prose document:

```rust
// crates/squeezy-agent/src/context_compaction.rs:903-915
let mut lines = Vec::new();
lines.push(format!(
    "Squeezy compacted conversation context (generation {generation})."
));
lines.push(
    "Preserve these durable facts, decisions, pinned entries, seen-file receipts, and unresolved questions; do not ask for raw output already summarized here unless it is needed again."
        .to_string(),
);
if let Some(summary) = &state.summary {
    lines.push(format!(
        "Previous compacted summary: {}",
        compact_text(summary, COMPACTION_PREVIOUS_SUMMARY_MAX_CHARS)
    ));
}
```

Then, in order: pinned context (each pin truncated to
`COMPACTION_PIN_SUMMARY_MAX_CHARS`), recent observations from the
`notes_recall` store (top 5), durable facts mined from the dropped slice
(`durable_context_lines`, capped at `COMPACTION_DURABLE_LINES_LIMIT = 24`),
unresolved questions (capped at `COMPACTION_UNRESOLVED_LINES_LIMIT = 8`),
active attachments, tool-output receipts, and finally
`<read-files>`/`<modified-files>` blocks from `file_lineage_blocks`
(`:1018`) that carry forward file-touch history across compaction
generations by re-parsing the prior summary's XML tags.

The final blob is bounded one more time by
`context_attachment_preview(..., config.context_compaction.max_summary_bytes)`
(`:991`), with `max_summary_bytes` defaulting to 12,000 bytes
(`squeezy-core/src/lib.rs:293`).

### Optional LLM-assisted rewrite

When `strategy` is `ModelAssisted` or `LayeredFallback`, the extractive
summary runs first, then a cheap model rewrites it into the four-slot
template:

```rust
// crates/squeezy-agent/src/context_compaction.rs:776-780
pub(crate) const STRUCTURED_COMPACTION_SYSTEM_PROMPT: &str = "You compact conversation context into a structured checkpoint. \
Update the existing summary in place — preserve every prior decision, \
progress entry, and next-step. Never invent new facts. Output only the \
four required sections in this exact order: `## Goal`, `## Progress`, \
`## Decisions`, `## Next`.";
```

The prompt is built by `build_structured_compaction_prompt` (`:807-855`)
and surfaces the *prior* compaction's summary as a separate
`<previous-summary>` block alongside the freshly extracted
`<new-conversation>` block, so the model updates slots iteratively
instead of re-truncating a chain on every round. The detection contract
`is_structured_compaction_summary` (`:872-893`) accepts any markdown
heading containing the slot keyword as a whole word — `### Goal`,
`## Key Decisions`, `## Next Steps` all pass — but if any of the four
slots is missing the output is rejected and the deterministic extractive
summary stays in place verbatim.

The fallback path is hard-coded for cost safety: timeouts
(`model_assisted_timeout_secs`, default 30s), empty responses, missing
slots, and any provider error all log a `compaction_fallback` session
event and return the extractive report unchanged (`:710-758`).
`LayeredFallback` further restricts the LLM call to "big enough"
dropouts — the dropped slice must exceed
`layered_fallback_extractive_threshold_tokens` (default 4,000) for the
model-assisted rewrite to be worth the round trip.

### Compaction history and undo

Every successful compaction appends a `ContextCompactionRecord` to
`state.history`. The list is capped:

```rust
// crates/squeezy-agent/src/context_compaction.rs:319-323
state.history.push(record.clone());
if state.history.len() > COMPACTION_MAX_HISTORY {
    let excess = state.history.len() - COMPACTION_MAX_HISTORY;
    state.history.drain(0..excess);
}
```

`COMPACTION_MAX_HISTORY = 20` (`:119`). The oldest records age out via
`drain(0..excess)` so a long session can still render a recent timeline
without unbounded growth.

Each compaction also writes a redb checkpoint keyed by
`ckpt-<generation>-<timestamp>`:

```rust
// crates/squeezy-agent/src/context_compaction.rs:280-304
let compacted_at_ms = unix_timestamp_millis();
let replacement_id = store.and_then(|store| {
    if dropped.is_empty() {
        return None;
    }
    let id = format!("ckpt-{generation}-{compacted_at_ms}");
    let checkpoint = squeezy_store::CompactionCheckpoint {
        replacement_id: id.clone(),
        session_id: String::new(),
        generation,
        items: dropped.clone(),
        created_unix_millis: compacted_at_ms as u128,
    };
    match store.put_compaction_checkpoint(&checkpoint) {
        Ok(()) => Some(id),
        Err(err) => {
            tracing::warn!(
                target: "squeezy::compaction",
                error = %err,
                "failed to persist compaction checkpoint; undo will be unavailable",
            );
            None
        }
    }
});
```

The checkpoint write is *best-effort*. A redb hiccup logs a warning and
leaves `replacement_id = None` (no undo for that compaction) rather than
aborting the compaction itself, because the cost saving from compacting is
load-bearing — losing the summary just because undo is unavailable would
be worse than losing undo.

`Agent::compact_context_undo` (`crates/squeezy-agent/src/lib.rs:2875-2933`)
walks the linkage in reverse: read `state.context_compaction.last`, look up
its `replacement_id` in the store, drop the synthetic summary at index 0,
prepend the restored items, decrement `generation`, pop the last history
record, and clear `previous_response_id` so the next request rebuilds from
the restored conversation.

## Worked example

Imagine a 30-turn debugging session against a 200K-token Claude model with
defaults: `min_items=16`, `estimated_tokens=60_000`, `recent_items=10`,
`threshold_percent=80`, `model_context_window=200_000`,
`max_summary_bytes=12_000`.

**Turns 1–10 (warm-up).** The user pastes a stack trace, the agent calls
`grep` and `read_file` to triangulate. Conversation grows to 22 items
(~35K tokens). Neither trigger fires: 22 > `min_items=16` is true but
35K < `estimated_tokens=60_000` is false, so `maybe_compact_conversation`
returns `None` at `context_compaction.rs:189`.

**Turn 15 (post-turn pass fires).** Two `read_file` calls against ~15 KB
sources push the conversation to 38 items / ~72K tokens. After turn 15's
assistant message lands, `maybe_compact_conversation` evaluates: both
gates open. It calls `compact_conversation` with `force=false` and
`trigger=Auto`. `recent_items=10` means the tail ten items (the last two
user turns, four `FunctionCall`/`FunctionCallOutput` pairs, and the
assistant turns) stay verbatim; the head 28 items become `older`. The summary
emitted at index 0 carries: a one-line preamble naming generation 1, the
"preserve these durable facts" pragma, a `notes_recall` block if
applicable, durable facts mined from the older slice (up to 24 lines),
attachment previews if any, and finally an alphabetised `<read-files>` /
`<modified-files>` block listing every path touched in turns 1–13.
Conversation drops from 38 items to 11 (1 summary + 10 recent). Estimated
tokens fall from ~72K to ~17K. The dropped 28 items persist as
`ckpt-1-<ms>`.

**Turns 16–24.** Work continues. Tool outputs accumulate again. By turn
24 the conversation is back to 33 items / ~85K tokens; the post-turn
trigger fires a second time. The new summary references "Previous
compacted summary:" (the truncated text of generation 1's summary, bounded
to `COMPACTION_PREVIOUS_SUMMARY_MAX_CHARS = 1200`) and re-parses the
prior `<read-files>` / `<modified-files>` blocks to seed lineage carried
across generations (`context_compaction.rs:1027-1038`). Generation
becomes 2.

**Turn 25 (mid-turn pass fires).** The user attaches three files and the
agent runs a 60K-token `decl_search`. Mid-stream, the provider's usage
event reports `last_total_tokens = 165_000`. The mid-turn checker computes
`threshold = 200_000 * 80 / 100 = 160_000`; observed 165K >= 160K so the
gate opens and `compact_conversation` runs *before* the request completes
its next turn. The forced flag is set: even with only 8 items above the
recent floor, the compaction proceeds. The agent's next streaming request
ships the new summary head plus the six recent items, not the 165K-token
balloon.

**Turn 28 (user reverts via undo).** The user types `/compact-undo`.
`compact_context_undo` resolves `state.context_compaction.last` (the
generation-3 record from turn 25), pulls `ckpt-3-<ms>` from the store,
asserts the conversation's head is currently a `UserText` summary
(`lib.rs:2895`), drops that head, prepends the restored 25 items, and
decrements `generation` to 2. The post-turn-25 messages stay attached on
the tail. The next request ships pre-turn-25 verbatim context plus turns
26–27 verbatim — the user gets back what the summary had compressed.

**The prompt the summarizer receives.** When the strategy is
model-assisted, the cheap model sees this system prompt (`context_compaction.rs:776`):

```
You compact conversation context into a structured checkpoint.
Update the existing summary in place — preserve every prior decision,
progress entry, and next-step. Never invent new facts. Output only the
four required sections in this exact order: `## Goal`, `## Progress`,
`## Decisions`, `## Next`.
```

Coupled with the user-role prompt assembled by
`build_structured_compaction_prompt` (`:807-855`), the model sees: a
`<new-conversation>` block (the extractive summary it must rewrite), an
optional `<previous-summary>` block (the prior generation's output), a
literal template showing each `##` heading, and a "Rules" section
demanding preservation of every prior entry, verbatim file paths and
function names, no invented facts, and a `<= max_output_tokens` budget
(default 1_500 from `DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS`).

## Edge cases & limits

**Never-summarized items.** `recent_items` (default 10) tail items pass
through untouched. Pinned entries persist on
`ContextCompactionState.pinned` (`squeezy-core/src/lib.rs:9642`) and are
re-emitted verbatim in every summary's "Pinned context:" block
(`context_compaction.rs:917-927`). Pins are added through
`Agent::pin_context_entry` (`lib.rs:2935`), live across compactions and
sessions via the resume-state write, and are sized by truncation
(`label` → 80 chars, `summary` → 800 chars).

**Media stripping.** Base64 image and PDF data URIs are replaced with
`[image]` / `[document]` (`:430-432`) before the older slice reaches the
summarizer, so a 300 KB inlined PNG cannot bloat the summary or push the
LLM-assisted compaction call itself into prompt-too-long. Importantly,
the stripping is local to the summary input — the persisted `dropped`
checkpoint receives the unmodified `older` (`:248`, `:273-274`), so
`compact_context_undo` restores the original bytes.

**Compaction history cap.** The in-memory `history` list is capped at
`COMPACTION_MAX_HISTORY = 20` records (`:119`). The cap is in-memory only;
it bounds rendering of the timeline and the cost of cloning state, not
disk persistence (per-checkpoint redb rows are governed by separate
retention).

**Tool-call/output integrity.** `snap_compaction_split` (`:350`) and
`drop_orphan_function_call_outputs` (`:377`) together guarantee the
post-compact conversation cannot reference a `call_id` whose declaring
`FunctionCall` was dropped. The symmetric repair pass
`repair_orphan_function_calls` (`:404`) injects a synthetic error output
when a `FunctionCall` lacks its matching output (e.g. after a cancelled
turn), because Anthropic's Messages API rejects any turn where a
`tool_use` block lacks a `tool_result`.

**Undo stash mechanics.** The "stash" isn't a separate in-memory buffer;
it's the redb `compaction_checkpoints` table keyed by `replacement_id`.
Each report carries both `dropped` (the pre-compact slice) and
`post_compact` (the new conversation), and the session event field is
stamped with `post_compact` so `replay_resume_state` snaps to the
post-compact checkpoint and forward-replays only strictly-newer events
(`context_compaction.rs:127-136`).

**Summarizer guardrails.** The structured prompt is explicit:
*"Do NOT invent new facts. Do NOT omit prior decisions."* (`:850`).
Detection at `is_structured_compaction_summary` (`:872-893`) catches the
single failure mode the template exists to prevent — a model output that
silently drops one of the four slots. When detection fails the extractive
summary stays verbatim and a `compaction_fallback` session event records
the reason: `model_assisted_timeout`, `model_assisted_error`,
`model_assisted_empty`, or `model_assisted_missing_slots`.

**File lineage caps.** Each of `<read-files>` and `<modified-files>` is
capped at `COMPACTION_FILE_LINEAGE_LIMIT = 50` entries (`:111`, `:1070-1077`).
When a list overflows the cap, the *chronologically oldest* paths are
dropped first so the most recent file touches survive. Modification
dominates: a file appearing in both read- and modify-class tool calls is
reported only under `<modified-files>` (`:1066-1068`).

## Cost intuition

Take the same 30-turn session against a 200K-window Claude model, each
turn appending ~5 KB of new context.

**Without compaction:**

| Turn | Cumulative input bytes | Sent on this turn |
|------|------------------------|-------------------|
| 10   | 50K                    | 50K               |
| 20   | 100K                   | 100K              |
| 30   | 150K                   | 150K              |

Cumulative input across all 30 turns: ~2.25 MB ≈ 560K tokens.

**With defaults (compaction at ~60K-token mark and every ~12 turns
thereafter):**

| Turn | Sent on this turn (approx) | Notes |
|------|----------------------------|-------|
| 10   | 50K                        | Below `estimated_tokens=60_000` floor — no compaction. |
| 15   | 72K → compact → 17K head + 5K next turn ≈ 22K | First auto-compact fires. |
| 24   | ~30K turn build-up, then compact again | Generation 2; chain reuses generation-1 summary header. |
| 30   | ~35K                       | Steady-state per-turn input ~25-35K. |

Cumulative input across all 30 turns post-compaction: roughly 600 KB
≈ 150K tokens — a 60-80% reduction relative to the no-compaction baseline,
matching the chapter framing. The model-assisted compaction call itself
costs ~3K input + 500 output tokens, fired at most once per compaction
generation against a cheap model (default `resolved_small_fast_model()`
when `model_assisted_model` is unset), so its incremental bill is well
under 1% of the savings.

**Prompt-cache interaction.** Providers cache by prefix: each new turn
that ships a strictly-extended conversation pays only for the suffix.
Compaction *breaks* that cache prefix — generation 2 replaces items 1–N
with a brand-new summary string, so the next request after compaction
pays the full prefix tokens again. The trade-off is intentional and the
math is in Squeezy's favor: a single full-prefix re-cache costs ~17K
tokens on the post-compaction conversation versus the 50K+ tokens that
would have been billed every subsequent turn under the uncompacted
trajectory. After the post-compaction re-cache the prefix grows steadily
again until the next compaction, so the amortised cost of a single
"cache-break" is one extra full prefix re-charge per ~10 turns instead of
the full input growing linearly each turn.

The two-tier strategy compounds with mid-turn compaction: when the
provider's running usage estimate spikes to 80% of the window mid-stream,
the agent compacts *before* the next request fires, so the cache break
happens at a moment where the alternative would have been a hard rejection
from the provider — i.e., no completion at all. In that branch the cost
saving is infinite by construction.
