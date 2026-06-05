# Cost-Saving Methodology

This note summarizes cost-saving mechanisms that are implemented in the repo
and can support public website copy. It is based on local code and docs only;
no network sources were used.

## Short Version

Squeezy reduces model spend by doing more deterministic work locally, sending
smaller evidence packets to the model, and keeping stable prompt bytes stable
enough for provider caches to work. The most defensible public claim is not a
fixed savings percentage. It is that Squeezy is designed to avoid paying a
large model to rediscover facts that local parsers, indexes, bounded tools,
receipts, compaction, and cheap-model routing can handle.

## Implemented Methodology

- **Local semantic graph and navigation.** Squeezy builds a tree-sitter-backed
  semantic graph for supported languages and exposes graph navigation tools
  such as `definition_search`, `reference_search`, `symbol_context`,
  `repo_map`, and `read_slice`. Symbols carry spans, signature/body ranges,
  provenance, confidence, and freshness, so the model can request a focused
  slice instead of a whole file. Unsupported or partial language cases surface
  fallback status rather than pretending graph confidence exists.
- **Bounded tool outputs.** File, grep, shell, graph, and web outputs are
  capped, shaped, deduped, or spilled with a recovery path. Examples include
  grep/file line truncation, shell output shaping for cargo/test output,
  `read_tool_output` spill recovery, image size limits, graph result limits,
  and aggregate packing of tool results.
- **Prompt caching support.** LLM requests carry a `CacheSpec`/retention model.
  Provider adapters emit the right cache hints where supported: Anthropic-style
  `cache_control`, OpenAI `prompt_cache_key` and long-retention
  `prompt_cache_retention`, Bedrock cache points, and provider usage parsing
  for cache-read/cache-write accounting where the wire format exposes it.
- **Lazy tool schemas and skill bodies.** Tool schemas default to lazy loading:
  core tools stay attached, discoverable tools are listed in a compact
  `<tools_index>`, and `load_tool_schema` attaches full schemas on demand.
  Active skills similarly render metadata by default and tell the model to call
  `load_skill` when it needs the full body. MCP tool schemas are compacted by
  pruning null/empty fields and unreachable `$defs`.
- **Conversation compaction and micro-compaction.** Full compaction summarizes
  older conversation items behind a bounded summary while keeping recent items
  and checkpoints. Micro-compaction fires earlier and rewrites older large
  tool outputs to placeholders while preserving call IDs, so stale bulk stops
  riding in every following request.
- **Cheap-model routing and rerouting.** A turn router can route obvious
  mechanical turns to a provider's small-fast model using a strict heuristic
  and optional judge. The same turn can escalate back to the parent model on
  tool-count, error, budget-denial, or low-confidence signals. Users can force
  `/cheap`, `/parent`, or toggle the router.
- **Subagent isolation.** Delegate/explore/plan/review/doc-help subagents run
  in their own conversation, usually with read-only or role-scoped tools. The
  parent receives a compact summary, receipts, files touched, and cost metrics,
  not the child's whole transcript unless transcript inclusion is enabled.
- **Accounting surfaces.** `CostSnapshot`, session metrics, `/cost`, and
  `/context` record provider cost, cache hits, cached writes where available,
  routing savings estimates, and local conversation shape. This matters because
  the product can say where tokens went without claiming perfect provider-side
  insight.

## Public-Site Claims We Can Safely Make

- Squeezy spends local CPU before paid model context by using tree-sitter
  parsing and a semantic navigation graph.
- Squeezy asks the model for focused code slices and graph packets instead of
  making whole files the default unit of understanding.
- Tool outputs are deliberately bounded and recoverable: large results are
  summarized, truncated, deduped, or spilled with a way to fetch the full bytes.
- Prompt-cache support is built into provider adapters and paired with
  byte-stable prompt assembly where possible.
- Tool schemas and skill bodies are lazy-loaded so rarely used instructions do
  not have to be sent on every request.
- Squeezy keeps long sessions bounded with compaction and earlier
  micro-compaction of stale tool-output bulk.
- Obvious mechanical turns can run on cheaper small-fast models, with same-turn
  escalation back to the parent model when the cheap path shows stress.
- Subagents isolate exploration work and return compact evidence to the parent
  instead of permanently inflating the main transcript.

## Claims To Avoid

- Avoid fixed public savings like "pays 5-15% of a naive agent bill" unless a
  dated benchmark methodology and comparison target are published next to it.
- Avoid "guaranteed cheaper" or "always cheaper." The graph can lose on small
  single-file tasks, and internal docs explicitly track cost-loss cases.
- Avoid "compiler-perfect," "LSP-equivalent," or "full semantic
  understanding." The graph is tree-sitter/local-analysis first, with
  confidence labels and documented limitations.
- Avoid saying every language has the same precision. Language docs list
  different indexed facts, oracle status, and known limitations per family.
- Avoid implying prompt caching is universal. It depends on provider/model
  capability and what the provider exposes on the wire.
- Avoid claiming OpenAI exposes cache-write tokens; current accounting records
  OpenAI cached-input tokens but no cache-write field.
- Avoid saying skills are plugins, marketplace extensions, or remote runtime
  add-ons. The documented scope is local filesystem instruction bundles.
- Avoid saying subagents are free or invisible. They spend tokens in child
  contexts; the saving is parent-context isolation and scoped tool/schema use.

## Concrete Messaging Bullets

1. "Understand the code locally before asking the model."
2. "Use graph navigation and focused slices instead of defaulting to whole-file reads."
3. "Keep tool output small, structured, and recoverable."
4. "Cache stable prompt prefixes when the provider supports it."
5. "Load tool schemas and skill instructions only when they are needed."
6. "Compact long sessions before old tool dumps become permanent baggage."
7. "Route simple mechanical turns to cheaper models, then escalate when the task stops being simple."
8. "Use subagents for isolated research, and return evidence instead of transcript bulk."

## Evidence Reviewed

- `docs/THESIS.md`
- `docs/internal/cost-saving/README.md`
- `docs/internal/cost-saving/01-provider-prompt-caching.md`
- `docs/internal/cost-saving/02-conversation-compaction.md`
- `docs/internal/cost-saving/03-tool-output-dedup-and-receipts.md`
- `docs/internal/cost-saving/04-structured-tool-output.md`
- `docs/internal/cost-saving/05-ast-code-retrieval.md`
- `docs/internal/cost-saving/06-lazy-schema-loading.md`
- `docs/internal/cost-saving/08-sub-agent-isolation.md`
- `docs/internal/cost-saving/10-token-accounting.md`
- `docs/internal/cost-saving/11-cheap-model-fast-path.md`
- `docs/internal/cost-saving/13-graph-retrieval-in-practice.md`
- `docs/internal/SKILLS_SCOPE.md`
- `crates/squeezy-skills/external-docs/LANGUAGES.md`
- `crates/squeezy-llm/src/cache_policy.rs`
- `crates/squeezy-llm/src/anthropic.rs`
- `crates/squeezy-llm/src/openai.rs`
- `crates/squeezy-llm/src/bedrock.rs`
- `crates/squeezy-llm/src/google.rs`
- `crates/squeezy-llm/src/registry.rs`
- `crates/squeezy-core/src/lib.rs`
- `crates/squeezy-agent/src/context_compaction.rs`
- `crates/squeezy-agent/src/micro_compaction.rs`
- `crates/squeezy-agent/src/turn_router.rs`
- `crates/squeezy-agent/src/subagent_catalog.rs`
- `crates/squeezy-agent/src/roles.rs`
- `crates/squeezy-agent/src/lib.rs`
- `crates/squeezy-tools/src/lib.rs`
- `crates/squeezy-tools/src/file_ops.rs`
- `crates/squeezy-tools/src/graph_tools.rs`
- `crates/squeezy-tools/src/shell_output.rs`
- `crates/squeezy-tools/src/shell_spillover.rs`
- `crates/squeezy-tools/src/truncate.rs`
- `crates/squeezy-store/src/lib.rs`
- `crates/squeezy-graph/src/lib.rs`
- `crates/squeezy-graph/src/references.rs`
- `crates/squeezy-parse/src/lib.rs`
- `crates/squeezy-rank/src/symbol_rank.rs`
- `crates/squeezy-skills/src/lib.rs`
- `crates/squeezy-mcp/src/lib.rs`

## Honest Limitations

- The semantic graph is strongest when the answer spans files or symbols. For
  small single-file tasks, a bounded read can be cheaper than many graph slices.
- Several language behaviors remain heuristic: dynamic dispatch, macros,
  generated code, reflection, overload/type solving, framework magic, and
  runtime import behavior are not compiler-equivalent.
- Provider caching only helps when the provider/model supports it and the
  request prefix stays stable enough to hit.
- Compaction trades verbatim old context for summaries and receipts. It lowers
  prompt size but can require re-fetching old detail.
- Cheap routing is deliberately conservative and can still misclassify; the
  implementation includes escalation and user overrides because the cheap path
  is not always sufficient.
- Subagents reduce parent transcript growth, but child contexts still cost
  model tokens and are bounded by configured concurrency, tools, and summary
  limits.
- The code has accounting and estimates, but public cost percentages need
  dated benchmark setup, provider prices, task mix, and comparison baselines.
