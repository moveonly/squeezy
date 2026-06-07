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

Squeezy's compaction subsystem reduces the older portion of the conversation
in escalating steps before the next request goes out. The cheaper steps clear
verbose tool-output bytes in place; the lossy step swaps the older slice for a
synthetic summary item. The trade-off of that lossy step is explicit: the
model loses verbatim recall of older tool outputs, gaining a structured prose
summary plus carry-forward file lineage. Pinned items and the most recent N
turns stay verbatim, so the model never has to guess at the immediate working
state.

## Mechanism

### The reduction ladder

Squeezy applies four reduction mechanisms in escalation order. Each is
strictly cheaper and less lossy than the next, so the agent reaches for the
expensive ones only under real pressure.

1. **Expired-read masking** — after a *successful* edit, the changed spans are
   spliced out of earlier `read_file`/`read_slice`/`grep` snapshots of the
   same file (`mask_expired_reads_after_edits`). It runs unconditionally after
   edits with no token gate and no model call, because a snapshot the edit
   just invalidated is pure dead weight.
2. **Trim** (the micro pass) — `maybe_micro_compact` clears older bulky
   `FunctionCallOutput` bodies (file reads, shell, web, graph navigation) into
   a placeholder *in place*, keeping every `FunctionCall`/`FunctionCallOutput`
   pairing by `call_id` and the newest `micro_compaction_keep_recent` (default
   5) outputs verbatim. It is cheap and structure-preserving, and it runs in
   two places: **between tool rounds** (mid-turn, gated by `enabled_mid_turn`)
   and as a **pre-pass at the post-turn boundary**, immediately before the
   summarize gate is evaluated. It fires at `trim_threshold()` —
   `trim_at_percent` (default 40%) of the effective window.
3. **Summarize** (the full tier) — `compact_conversation` condenses the older
   slice into a single synthetic summary item, keeps the newest `recent_items`
   (default 10) verbatim, and is reversible via `/compact undo`. It runs
   **only** at the post-turn boundary (after the trim pre-pass) or via the
   forced-overflow path; it never fires mid-turn. It triggers at
   `summarize_threshold()` — `summarize_at_percent` (default 95%) of the
   effective window, held at least `DEFAULT_CONTEXT_OUTPUT_HEADROOM_TOKENS`
   (16K) below the window so the next reply still fits.
4. **Forced overflow** — a reactive emergency summarize when the provider
   returns a context-window error mid-request
   (`try_provider_context_overflow_compaction`). This is the last resort, run
   with `force=true` so it shrinks even a few-but-enormous conversation.

Mid-turn pressure is handled by trim alone. The lossy summary head is only ever
produced at the turn boundary or by the forced-overflow path, so a long
tool-heavy turn reclaims bytes between rounds without committing to a summary
the user cannot inspect until the turn is done.

```rust
// crates/squeezy-agent/src/context_compaction.rs
pub(crate) async fn maybe_compact_conversation(
    conversation: &mut Vec<LlmInputItem>,
    /* state, attachments, store, provider, session, redactor, */
    config: &AppConfig,
    trigger: ContextCompactionTrigger,
    overhead_tokens: u64,
) -> Option<ContextCompactionReport> {
    if !config.context_compaction.enabled {
        return None;
    }
    let cc = &config.context_compaction;
    let estimate = estimate_context(conversation);
    // estimate_context omits the system instructions and tool schemas that
    // ride along on every request; fold in the caller's measured overhead.
    let tokens = estimate.estimated_tokens.saturating_add(overhead_tokens);
    // Lossy summarize fires only once usage crosses the window-relative
    // summarize threshold; the cheap trim pre-pass has already run.
    let ceiling = cc.summarize_threshold();
    let over_high_water = tokens >= cc.min_items_bypass_threshold();
    if (estimate.items < cc.min_items && !over_high_water) || tokens < ceiling {
        return None;
    }
    compact_conversation_with_strategy(/* … */, trigger, false).await
}
```

### Thresholds are fractions of the effective window

Every threshold resolves against the **effective window**, defined by shared
`ContextCompactionConfig` helpers (`squeezy-core/src/lib.rs`) so the TUI nudge,
the trim gate, and the summarize gate agree on every model:

- `resolve_window()` returns `model_context_window` when the model registry
  knows it, otherwise `fallback_window_tokens` (default **128_000**). The
  fallback is *only* used for unknown windows — there is no flat token budget
  capping the trigger on known windows.
- `effective_window()` is `min(resolve_window(), max_context_tokens)` when the
  optional, opt-in `max_context_tokens` economy cap is set, otherwise just
  `resolve_window()`. Capping the effective window keeps the trim → summarize
  ladder ordered even under a low cap.
- `trim_threshold()` = `trim_at_percent` (40) of the effective window.
- `warn_threshold()` = `warn_at_percent` (85) of the effective window — drives
  the pre-summarize nudge.
- `summarize_threshold()` = `summarize_at_percent` (95) of the effective
  window, clamped so it never sits closer to the window than the 16K output
  headroom and never below `trim_threshold() + 1`.

The token count the summarize gate compares is the conversation estimate
**plus** the instruction + tool-schema overhead from the most recent request
(`overhead_tokens`), so a tool-heavy config does not under-count the real
input. The `min_items` floor is bypassed once the conversation crosses
`min_items_bypass_threshold()` (≈90% of the window), so a "few but enormous"
conversation still summarizes proactively. The mid-turn trigger prefers the
provider's reported `last_total_tokens` and falls back to the byte-derived
estimator otherwise.

Resolved trigger points for the defaults (trim 40 / warn 85 / summarize 95):

| window | trim | warn | summarize |
|--------|------|------|-----------|
| 128K (fallback) | 51.2K | 108.8K | 112K |
| 270K | 108K | 229.5K | 256.5K |
| 1M | 400K | 850K | 950K |

The post-turn entry point delegates through `compact_conversation_with_strategy`
to `compact_conversation` (`context_compaction.rs`), which holds the splitting
logic.

### Splitting older from recent

```rust
// crates/squeezy-agent/src/context_compaction.rs
let before = estimate_context(conversation);
let mut keep = config.context_compaction.recent_items.max(1);
if !force
    && before.estimated_tokens >= config.context_compaction.min_items_bypass_threshold()
{
    // Few-but-enormous: with the default recent_items (10) a handful of huge
    // items would keep everything verbatim and fold nothing, making the
    // post-turn min_items bypass inert. Over the high-water mark, cap keep so
    // at least half the items form a foldable older slice. The no-op guard
    // (after.bytes >= before.bytes) still declines if it can't actually shrink.
    keep = keep.min(before.items / 2).max(1);
}
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
defaults: `min_items=16`, `recent_items=10`, `micro_compaction_keep_recent=5`,
`trim_at_percent=40`, `warn_at_percent=85`, `summarize_at_percent=95`,
`model_context_window=200_000`, `max_summary_bytes=12_000`. The registry knows
this window, so the thresholds resolve to trim at 80K, warn at 170K, and
summarize at 184K (95% of 200K is 190K, clamped to 16K below the window).

**Turns 1–10 (warm-up).** The user pastes a stack trace, the agent calls
`grep` and `read_file` to triangulate. Conversation grows to 22 items
(~35K tokens). Nothing fires: 35K sits below the 80K trim threshold, so neither
the trim pre-pass nor the summarize gate engages.

**Turn 15 (trim fires).** Two `read_file` calls against ~15 KB sources push
the conversation to 38 items / ~92K tokens, crossing the 80K trim point. The
mid-turn trim pass clears the older bulky `FunctionCallOutput` bodies in place,
keeping the newest five tool outputs verbatim and preserving every `call_id`
pairing. The summarize gate stays shut — 92K is far below 184K — so no summary
head is produced. The cleared tool bytes drop the request size back toward
~45K while every turn and tool call still appears in order.

**Turns 16–28 (steady-state trimming).** Work continues; each tool-heavy turn
that crosses the trim threshold reclaims its older outputs in place. The
conversation keeps its full structure and the model never loses verbatim recall
of *recent* work, because trim only touches outputs older than the kept tail.

**Turn 29 (warn nudge, then summarize).** A burst of large `decl_search` and
`read_file` results pushes usage to ~172K, inside the warn band. The TUI
surfaces the pre-summarize nudge once (more items than `recent_items`, so a
summarize would actually shrink), telling the user older turns are about to be
summarized and that `/compact undo` reverses it. The next turn lands at ~186K,
crossing the 184K summarize threshold. After the turn's assistant message
lands, the post-turn trim pre-pass runs first, then `maybe_compact_conversation`
evaluates: items ≥ `min_items` and tokens ≥ `summarize_threshold()`, so it
calls `compact_conversation` with `force=false` and `trigger=Auto`.
`recent_items=10` means the tail ten items stay verbatim; the head items become
`older`. The summary emitted at index 0 carries: a one-line preamble naming
generation 1, the "preserve these durable facts" pragma, a `notes_recall`
block if applicable, durable facts mined from the older slice (up to 24 lines),
attachment previews if any, and finally an alphabetised `<read-files>` /
`<modified-files>` block listing every path touched in the dropped slice.
Estimated tokens fall from ~186K to ~17K. The dropped items persist as
`ckpt-1-<ms>`.

**Forced overflow (alternative path).** Had a single oversized tool result
spiked the request past the provider's hard limit before the post-turn gate
ran, the provider would return a context-window error and
`try_provider_context_overflow_compaction` would summarize reactively with
`force=true` — even a few-but-enormous conversation shrinks — and retry the
request. In that branch the alternative is a hard rejection with no completion
at all.

**Turn 30 (user reverts via undo).** The user types `/compact undo`.
`compact_context_undo` resolves `state.context_compaction.last`, pulls
`ckpt-1-<ms>` from the store, asserts the conversation's head is currently a
`UserText` summary (`lib.rs:2895`), drops that head, prepends the restored
items, and decrements `generation`. The post-summarize messages stay attached
on the tail. The next request ships the restored verbatim context plus the
newer verbatim turns — the user gets back what the summary had compressed.

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

**Window re-derivation on model switch.** `model_context_window` is
auto-derived from the model registry at `Agent::build`, and `resolve_window()`
falls back to `fallback_window_tokens` (128K) only when the registry does not
know the active model. A runtime model switch (`replace_provider` /
`drain_pending_swap` / `replace_config`) re-derives it for the new model via
`re_derive_model_context_window`, so the trim, warn, and summarize thresholds
track the *new* model's window for the rest of the session. An explicit
override (`SQUEEZY_CONTEXT_MODEL_CONTEXT_WINDOW` or `[context]
model_context_window`) always wins and survives the switch.

**Editing the knobs.** Every `[context]` field is editable in the
interactive `/config → Context & Compaction` screen (all `NextPrompt`
tier), in addition to `squeezy.toml` and the `SQUEEZY_CONTEXT_*` env vars.
The section's `triggers` info row shows the effective window and the token
points each tier (trim / warn / summarize) fires at for the active model.

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

**With defaults (trim at 40% of the window, summarize near the top):**

| Turn | Sent on this turn (approx) | Notes |
|------|----------------------------|-------|
| 10   | 50K                        | Below the 80K trim threshold — full transcript. |
| 15   | 92K → trim → ~45K          | Crosses 40%-of-window trim; older tool bodies cleared in place, structure intact. |
| 16–28 | ~45–80K, re-trimmed       | Each tool-heavy turn that re-crosses the trim point reclaims its older outputs. |
| 29   | ~186K → summarize → 17K head + 5K next ≈ 22K | Crosses the summarize threshold; older slice folds into one summary item. |
| 30   | ~22K                       | Steady-state per-turn input after the fold. |

Trim alone keeps the per-turn bill well under the no-compaction trajectory for
most of the session; the lossy summarize fires once near the top of the window
to reset the prefix. Across all 30 turns the cumulative input lands far below
the ~560K-token uncompacted baseline. The model-assisted compaction call (only
on the summarize tier, and only when the strategy requests it) costs ~3K input
+ 500 output tokens, fired at most once per compaction generation against a
cheap model (default `resolved_small_fast_model()` when `model_assisted_model`
is unset), so its incremental bill is well under 1% of the savings.

**Prompt-cache interaction.** Providers cache by prefix: each new turn
that ships a strictly-extended conversation pays only for the suffix. The
two cheap tiers are prefix-friendly by design — expired-read masking and
trim rewrite *earlier* outputs into placeholders, so they do invalidate the
cached prefix, but they trade that one re-cache for a permanently smaller
prefix on every subsequent turn. The summarize tier *breaks* the prefix
harder: it replaces the whole older slice with a brand-new summary string,
so the next request pays the full post-summary prefix again. The trade-off
is intentional and the math is in Squeezy's favor: a single full-prefix
re-cache costs ~17K tokens on the post-summary conversation versus the
50K+ tokens that would have been billed every subsequent turn under the
uncompacted trajectory. After the re-cache the prefix grows steadily again
until trim or the next summarize fires, so the amortised cost of a single
"cache-break" is one extra full prefix re-charge instead of the full input
growing linearly each turn.

The ladder compounds with the forced-overflow path: if a request still
overflows the provider's hard limit, the agent summarizes reactively and
retries *before* giving up, so the cache break happens at a moment where
the alternative would have been a hard rejection — i.e., no completion at
all. In that branch the cost saving is infinite by construction.
