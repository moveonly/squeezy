# Squeezy Cost-Saving Architecture

Squeezy keeps LLM cost down through a layered set of mechanisms — provider-side prompt caching, conversation compaction, tool-output shaping, semantic code retrieval, lazy schema loading, persistent session memory, and disciplined sub-agent isolation. This directory is the canonical detailed audit for those mechanisms. The implementation details below were last reconciled with the current codebase in June 2026; benchmark snapshots and provider prices should still be rechecked before using them as current performance or billing claims.

Start with [`00-unique-cost-features.md`](00-unique-cost-features.md) for the
short "what makes Squeezy different" narrative. Use this index and the numbered
chapters for implementation details, source references, limits, and tuning
knobs.

Each chapter sits beside this index as a numbered file:

| # | Chapter | Layer | Primary files |
|---|---------|-------|---------------|
| [00](00-unique-cost-features.md) | Unique cost-efficiency features overview | Cross-cutting | Map of implemented layers and primary source files |
| [01](01-provider-prompt-caching.md) | Provider-side prompt caching | Request | `crates/squeezy-llm/src/cache_policy.rs`, per-provider modules |
| [02](02-conversation-compaction.md) | Conversation compaction | Conversation shape | `crates/squeezy-agent/src/context_compaction.rs` |
| [03](03-tool-output-dedup-and-receipts.md) | Tool-output dedup & receipt stubs | Conversation shape | `crates/squeezy-agent/src/context_compaction.rs`, `squeezy-store` |
| [04](04-structured-tool-output.md) | Structured tool-output extraction | Tool output | `crates/squeezy-tools/src/shell_output.rs`, `shell_spillover.rs`, `file_ops.rs` |
| [05](05-ast-code-retrieval.md) | Semantic AST-based code retrieval | Code-context selection | `crates/squeezy-parse`, `squeezy-graph`, `squeezy-rank`, `squeezy-tools/src/graph_tools.rs` |
| [06](06-lazy-schema-loading.md) | Lazy schema loading | Prompt assembly | `crates/squeezy-agent/src/lib.rs`, `crates/squeezy-skills/src/lib.rs` |
| [07](07-session-persistence-and-memory.md) | Session persistence & cross-session memory | Persistence | `crates/squeezy-store/src/sessions.rs`, `crates/squeezy-tools/src/checkpoint_provider.rs` |
| [08](08-sub-agent-isolation.md) | Sub-agent context isolation | Conversation shape | `crates/squeezy-agent/src/subagent_catalog.rs`, runner code in `squeezy-agent/src/lib.rs` |
| [09](09-verbosity-controls.md) | User-controllable verbosity | Output shape | `crates/squeezy-core/src/lib.rs:6469+`, TUI handlers |
| [10](10-token-accounting.md) | Token accounting & `/context` telemetry | Observability | `crates/squeezy-core/src/lib.rs:9805+`, per-provider extractors, `crates/squeezy-agent/src/lib.rs:510+` |
| [11](11-cheap-model-fast-path.md) | Cheap-model fast path (per-turn routing) | Request | `crates/squeezy-agent/src/turn_router.rs`, `crates/squeezy-core/src/lib.rs` (`RoutingConfig`) |
| [12](12-implemented-idea-batch.md) | Implemented idea batch (2026-06): signature_span, shell sidecar, pressure gate, per-role reasoning, expired-context masking | Multiple | `squeezy-parse`, `squeezy-tools`, `squeezy-agent`, `squeezy-llm` |
| [13](13-graph-retrieval-in-practice.md) | Graph retrieval in practice (build cost, streaming robustness, read routing) | Code-context selection | `crates/squeezy-graph`, `crates/squeezy-llm/src/retry.rs`, `crates/squeezy-tools/src/graph_tools.rs` |

---

## What the agent is paying for, by layer

Every LLM coding agent pays four kinds of bill on every turn:

1. **Request bytes** — the system prompt, the tool schemas, the conversation history, the user's new message. Billed at the provider's input rate unless something *caches* it.
2. **Tool output bytes** — every byte the model emits as a tool call plus every byte the tool returns. Billed at input rate on the next turn unless something *trims, dedups, or compresses* it.
3. **Output bytes** — what the model says. Billed at output rate (3-5× input). Cuttable only by *shrinking the prompt or telling the model to be terse*.
4. **Recomputation across turns/sessions** — anything the model has to re-derive because it forgot. Paid in extra turns.

Squeezy's architecture is a set of orthogonal mechanisms, each targeting one or two of those layers. The chapters are organised in roughly the order the bytes flow on a single turn — from "what's on the wire" inward.

## Layer map

```
┌─────────────────────────────────────────────────────────────┐
│ Request layer (per-API-call cost)                           │
│   Ch.01  Provider prompt caching (Anthropic, OpenAI, ...)   │
│   Ch.10  Token accounting (observability for every mech.)   │
│   Ch.11  Cheap-model fast path (per-turn routing)           │
├─────────────────────────────────────────────────────────────┤
│ Prompt-assembly layer (what goes into "system" + tools)     │
│   Ch.06  Lazy tool schemas + lazy skill bodies              │
│   Ch.09  Verbosity-controlled prompt fragments              │
├─────────────────────────────────────────────────────────────┤
│ Conversation-shape layer (what's in messages[])             │
│   Ch.02  Conversation compaction (mid-turn + post-turn)     │
│   Ch.03  Tool-output receipt stubs (SHA-keyed dedup)        │
│   Ch.08  Sub-agent context isolation                        │
├─────────────────────────────────────────────────────────────┤
│ Tool-output layer (what each tool returns)                  │
│   Ch.04  Structured extraction (cargo/test/grep/diff/imgs)  │
│   Ch.04  Spillover to tempfiles with read_tool_output       │
├─────────────────────────────────────────────────────────────┤
│ Context-selection layer (what the model retrieves)          │
│   Ch.05  Semantic AST graph (signatures + BM25 ranking)     │
├─────────────────────────────────────────────────────────────┤
│ Persistence layer (what survives across turns/sessions)     │
│   Ch.07  Checkpoint-anchored resume, fork, memory.md        │
└─────────────────────────────────────────────────────────────┘
```

## What each chapter covers

### 01 — Provider-side prompt caching

The cheapest token is one the provider never re-charges. Squeezy plants explicit cache breakpoints where the provider supports them — `cache_control: { type: "ephemeral", ttl: "1h" }` on Anthropic, `prompt_cache_key` + `prompt_cache_retention: "24h"` on OpenAI Responses, and typed `CachePoint` blocks on Bedrock. Google is narrower: Squeezy does not create a `cachedContent` resource, but it does read `usageMetadata.cachedContentTokenCount` when Gemini reports server-side cache hits. The cache-policy code in `crates/squeezy-llm/src/cache_policy.rs` decides *where* explicit breakpoints go (system tail, last stable tool, last user block) and the per-provider modules translate that intent to the wire format. The same chapter covers the deliberate exclusion of MCP tools from Anthropic-style stable-tool breakpoints so a `tools/list` refresh doesn't invalidate the cache.

### 02 — Conversation compaction

Compaction reduces the conversation in escalating steps keyed to fractions of the effective window (`min(model_context_window or fallback_window_tokens, max_context_tokens)`). A cheap trim pass clears older tool-output bodies in place at `trim_at_percent` (40%), running both between tool rounds and as a pre-pass at the turn boundary. Only at `summarize_at_percent` (95%) — post-turn, or reactively on a provider overflow error — does `compact_conversation` in `crates/squeezy-agent/src/context_compaction.rs` fold older items into a four-slot summary head (`## Goal / ## Progress / ## Decisions / ## Next`), keeping the recent N items and a pinned list intact. A `LayeredFallback` only pays for an LLM-assisted rewrite when the dropped span exceeds a token threshold. The original messages stay in a checkpoint so the user can `/compact undo`.

### 03 — Tool-output dedup and receipt stubs

Agents re-read the same file and re-grep the same pattern often. Squeezy hashes each tool's stable output (SHA256) and, on a second identical call, returns a `{ receipt_stub: true, same_as_call_id, original_output_sha256 }` instead of the bytes. The dedup runs at four scopes: per-tool inside `read_file` (`file_ops.rs:605–651`), in-conversation during compaction (`context_compaction.rs:1293–1615`), cross-session in `SqueezyStore::put_tool_receipt`, and aggregate-budget packing per round (`pack_tool_results` with `max_tool_result_bytes_per_round`). The chapter shows the actual JSON shape returned in each case.

### 04 — Structured tool-output extraction

Raw `cargo build` output is 50–500KB of noise. Squeezy ships hand-written shapers for cargo/rustc JSON, nextest, pytest, jest/vitest, and trimmed shaped blocks at 8000 chars with a `read_tool_output` recovery hint. Overflow gets written to per-session tempfiles under `$TMPDIR/squeezy-spillover/<session>/<sha-prefix>` with a 100MiB budget. Grep is capped at 2000 chars/line × 48KB total with `BTreeSet` dedup of paths. `diff_only` reads/globs skip clean files. Images are detected by magic bytes and base64-encoded with MIME so the model sees them as `LlmInputItem::Image` rather than mangled UTF-8.

### 05 — Semantic AST-based code retrieval

Tree-sitter parses every supported language family (Rust, Python, Java, Kotlin, Scala, C#, Go, C/C++, JavaScript/TypeScript, PHP, Ruby, Swift, and Dart) into typed AST nodes, but the cost saving comes from the semantic layer built on top. `ParsedSymbol { signature_span, body_span }` lets `read_slice {span_kind: "signature"}` return a declaration header without its body when the extractor can anchor the body boundary; bodyless or heuristic symbols fall back to the full declaration span. `squeezy-graph` cross-links symbols by call, reference, and container hierarchy with trigram prefilters; `squeezy-rank` ladders Exact → CaseInsensitive → SignatureSubstring → TokenBag → Fuzzy and reranks with BM25 (K1=1.2, B=0.75). The model retrieves through this index — `definition_search` for ranked candidates, `symbol_context` for callers + callees + refs as JSON, `repo_map` for hierarchy — instead of dumping whole files. Tree-sitter `edit()` + `changed_ranges()` keeps the index incremental so re-parses touch only changed regions.

### 06 — Lazy schema loading

Full tool JSON schemas are tens of KB per request and most aren't used. Squeezy splits tools into a small core set (always included) and a discoverable set advertised by name+description in a `<tools_index>` block, sorted alphabetically so the prefix stays byte-stable for prompt-cache reuse. A synthetic `load_tool_schema` control tool pulls a full schema on demand, and that schema sticks for the rest of the session. Skills follow the same pattern: `metadata_block` emits a stub with a "call `load_skill` for full instructions" hint, and `SkillCatalog::load` caches bodies once fetched.

### 07 — Session persistence and cross-session memory

Sessions live in `.squeezy/sessions/<id>/` as `metadata.json` + append-only `events.jsonl` + `resume_state.json` + pre-parsed `replay.jsonl`. Resume snaps to the newest `ContextCompacted` checkpoint and applies only newer events, so a 100-turn session resumes by paying for just the compacted head plus the tail. `fork_session` copies `resume_state.json` for an instant branch. Cross-session memory is a single `~/.squeezy/memory.md` file read once per session into base instructions, capped by `context_compaction.user_memory_max_bytes`. A global `~/.squeezy/sessions/index.jsonl` lets the resume picker find sessions across projects without scanning each. Edits are mirrored to a shadow git store by `JournalCheckpointProvider` so later turns can reference diffs instead of re-reading files.

### 08 — Sub-agent context isolation

A sub-agent starts with its own `system_prompt` + a restricted `tools` allowlist (`subagent_allowed_tools`) + its own model selection — none of the parent's transcript. `SUBAGENT_MAX_CONCURRENT = 20` and `DELEGATE_CHAIN_MAX_STEPS = 16` cap fan-out; a hard no-grandchild rule (enforced in `roles_tests.rs:42`) prevents recursive spawning. The parent sees only a `compact_text` summary bounded by `max_summary_tokens`, so a "would-be 30k-token parent expansion" becomes "3 × 8k sub-agent contexts + 3 × 0.5k returns" and runs in parallel.

### 09 — User-controllable verbosity

Three orthogonal axes. `ResponseVerbosity { Concise | Normal | Verbose }` swaps in different response-shape paragraphs in the system prompt; the `Normal` path short-circuits to zero token cost. `ToolOutputVerbosity { Compact | Normal | Verbose }` controls transcript/TUI rendering of tool results and does not shrink provider request bytes. `ShellDiffInline { Full | Folded }` folds large shell diffs into a preview hot-reloaded via a `SHELL_DIFF_INLINE_OVERRIDE` static. Native-API providers (OpenAI Responses) ride verbosity on the `text_verbosity` API parameter instead of the system prompt, so cache prefixes stay hash-stable across toggles.

### 10 — Token accounting and `/context` telemetry

`CostSnapshot { input_tokens, output_tokens, reasoning_output_tokens, cached_input_tokens, cache_write_input_tokens, estimated_usd_micros }` is the universal currency. Each provider's usage block is normalised back into it — Anthropic and Bedrock fold cache-read and cache-write back into the total `input_tokens`, OpenAI exposes `input_tokens_details.cached_tokens` only (no cache-write field), and Google reports `cachedContentTokenCount` when available. `SessionAccountingSnapshot` and `ConversationShape` break the local request down by user/assistant text, function-call bytes, tool-output bytes, reasoning bytes, image bytes, and attachment bytes; `/context` also calls out provider-stored context as an unknown exact current-window quantity when `store_responses=true`.

### 11 — Cheap-model fast path (per-turn routing)

Each user turn is classified before its first LLM round. A strict heuristic prefilter (single sentence, ≤ 15 words, tight imperative whitelist, no compound connectors, no ambiguity markers) admits the most obvious slam-dunks (`run cargo test`, `checkout main`, `grep TODO src/lib.rs`) and dispatches them on the provider's cheap tier resolved by `cheap_model_for`: `[providers.<id>].cheap_model`, then legacy `[model].small_fast_model`, then the provider's built-in judge/mini default. OpenAI/Azure therefore route easy turns to `gpt-5.4-mini` by default, while deployments can opt into `gpt-5.4-nano` with an explicit cheap-model override. Anything the heuristic does not catch within `judge_max_chars` defers to a one-shot JSON-constrained LLM judge. The judge prefers a provider-specific or configured `judge_model` (OpenAI/Azure default to `gpt-5.4-mini`), then falls back to the curated judge/cheap tier, uses `max_output_tokens = 512`, and leaves `reasoning_effort` unset. The cheap-routed turn is monitored mid-flight: tool-call ceiling (`max_tool_calls_per_turn / 4`), `tool_errors + budget_denials ≥ 2`, or a low-confidence assistant-text phrase ("i'm not sure", "this is complex", …) trips a same-turn handoff back to the parent model via `current_model` swap, plus an escalation-sticky window that forces the next 3 user prompts to skip the router. Image input always routes parent. `/cheap`, `/parent`, and `/router off|on` slash commands give the user manual control through `Agent::request_routing_force_*` and `Agent::set_routing_session_disabled`.

## Cross-cutting design principles

Reading the chapters in order, the same handful of principles recur:

1. **Byte-stable ordering wherever caching happens.** The `<tools_index>` is alphabetised. The system tail is deterministic. The MCP tool exclusion sits before the breakpoint specifically so dynamic MCP advertisements don't bump the prefix hash.
2. **Lazy materialisation, opt-in expansion.** Default to a stub or a name — the model asks for the full body when it wants it. Applied to tool schemas, skill bodies, AST bodies (`span_kind`), and tool-output spillover.
3. **Content-addressed dedup.** SHA256 keys appear in receipt stubs, in spillover filenames, in cross-session receipts, and in the file-window dedup. Re-derivation is always cheaper than re-transmission.
4. **Always emit a recovery path.** Every trim, stub, fold, or dedup carries an explicit "call X to get the full content" pointer. The model is never trapped by a saving.
5. **Snapshot anchors over linear history.** Compaction writes a checkpoint with the dropped items so undo is cheap; resume scans backward for the newest `ContextCompacted` so it never replays a long log; `JournalCheckpointProvider` snapshots before edits so diffs replace re-reads.
6. **User-visible, user-controllable knobs.** Verbosity sliders, `/context` telemetry, retention setting, `diff_only` flag, and slash-command overrides put the user in the loop on cost trade-offs rather than burying them.
7. **Honest accounting first.** Every saving is observable in `CostSnapshot` and `/context`. None of the mechanisms can be evaluated without the per-provider usage extractors in Chapter 10.

## Future ideas

The architecture leaves natural extension points where future work could squeeze out more savings:

- **Dense / embedding-augmented code search.** Supplement the lexical BM25 + trigram + tier ladder with semantic embeddings so `verify_token` matches "auth checking" queries. Would feed `definition_search` and `symbol_context`.
- **Grandchild sub-agents under governance.** Sub-agents cannot spawn more sub-agents today (`roles_tests.rs:42`). A budget-gated relaxation would enable deeper exploration trees without unbounded cost.
- **Mid-stream cancellation at tool-call boundaries.** The Anthropic streamer already parses `tool_use` events as they arrive; cancelling the rest of the stream once a tool call is committed would shave output tokens on long reasoning passes.
- **Static-memory consolidation.** `notes_remember` / `notes_recall` now cover model-callable durable observations, but they are separate from the static `~/.squeezy/MEMORY.md` prompt file. A governed consolidation path could summarize high-value notes back into static startup memory without letting arbitrary turns rewrite base instructions.
- **Retention sweeper for receipt/snapshot redb tables.** Content-hash keys make stale entries safe, but a TTL- or size-based sweeper would keep on-disk size bounded over very long projects.

Each idea links back to the chapter where the prerequisite plumbing already exists.

## How to read this set

- **If you want the headline numbers**, read each chapter's `## Cost intuition` section. They cite back-of-envelope per-mechanism savings.
- **If you're tuning a deployment**, the `## Edge cases & limits` sections list every configurable knob with its config key.
- **If you're porting an idea to a different agent**, the `## Mechanism` sections quote the actual Rust code and cite file:line so the design is reproducible.
- **If you're auditing the bill**, Chapter 10 (`Token accounting`) is the right entry point — it shows where every other mechanism surfaces in the numbers.

## Total savings, rough

Compounded across the layers, on a 30-turn coding session with 10-15 tool calls per turn:

| Layer | Without | With Squeezy | Savings |
|-------|---------|--------------|---------|
| Per-turn system + tools | ~30KB schemas + 5KB system | ~5KB system + 1KB tools index | -83% on prompt assembly |
| Provider cache hit | 0% reused | 60-80% of stable prefix cached | -50-65% of input price |
| Tool outputs | Raw cargo (200KB), full file reads | Shaped (4KB), signature-only, dedup stubs | -85-95% on tool bytes |
| Conversation grow-over-time | linear in turns | bounded by compaction head | -60-80% by turn 30 |
| Session resume | Replay 100k input tokens | Resume from compacted head | -90%+ on resume |

These multiply rather than add, since each acts at a different layer of the request. The order-of-magnitude headline: a Squeezy session typically pays 5-15% of the bill a naive agent of the same capability would pay.
