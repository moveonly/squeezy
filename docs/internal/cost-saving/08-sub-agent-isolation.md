# Sub-Agent Context Isolation

## Motivation

A research question that touches a dozen files normally costs the
parent transcript a dozen tool-call requests, a dozen tool-result
blobs, and an assistant chain reasoning over them. Every later turn
re-sends all of that as input context. If the parent only needed the
*conclusion*, every intermediate byte is bloat that prices in for the
rest of the session.

Squeezy pushes the research into a sub-agent: a child `Agent`
invocation with its own LLM stream, its own conversation buffer, its
own narrowed tool set, and its own short system prompt. The parent's
transcript receives only the sub-agent's compacted summary and a
small `supporting_receipts` trail — never the child's tool dumps or
chain-of-thought.

Two cost effects compound. First, parent context stays slim: a
30-tool-call investigation becomes a sub-kilobyte summary in the
parent, not 30 inlined tool outputs, and later parent turns re-send
the slim version forever. Second, the child's per-round prompt is
smaller: a sub-agent advertises only its declared tool subset, and
with lazy schema loading on, omits schemas of every tool the role
does not need. Both effects apply on top of caching, not in place
of it.

## Mechanism

### Sub-agent definitions live in a catalog

The catalog merges compile-time built-in kinds with user/project
`.md` files discovered under `~/.squeezy/agents/` (`USER_SUBAGENTS_DIR`,
`subagent_catalog.rs:39`) and `<workspace>/.squeezy/agents/`
(`PROJECT_SUBAGENTS_DIR`, `subagent_catalog.rs:35`). Project entries
shadow user entries, which shadow built-ins. Every definition carries
the four fields a runner needs — name, description, system prompt
body, plus optional model override and declared tools allowlist:

```rust
// crates/squeezy-agent/src/subagent_catalog.rs:81-93
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentDefinition {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    pub system_prompt: String,
    pub source: SubagentSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
}
```

The four built-in kinds (`delegate`, `explore`, `plan`, `review`) are
hard-coded in `builtin_entries` (`subagent_catalog.rs:440-481`) so
dispatch works on a fresh install. A fifth hidden kind, `doc_help`,
services `/help` from inlined bundled docs and is intentionally
absent from the catalog.

### Concurrency cap and step budget

Two integer caps gate fanout:

```rust
// crates/squeezy-agent/src/lib.rs:123-129
/// Hard cap on the number of steps a single `delegate_chain` call may
/// declare. Each step burns a full subagent lease + LLM round, so the
/// chain is intentionally narrower than the parent agent's per-turn tool
/// budget. Eight steps is enough to thread a non-trivial multi-stage
/// research workflow without letting the model commit the entire turn
/// budget to one chain.
const DELEGATE_CHAIN_MAX_STEPS: usize = 16;
```

```rust
// crates/squeezy-core/src/lib.rs
pub const DEFAULT_SUBAGENT_MAX_CONCURRENT: usize = 20;

// crates/squeezy-agent/src/lib.rs
context.config.subagents.max_concurrent.max(1)
```

Concurrent fanout uses `buffer_unordered(config.subagents.max_concurrent)`.
The default is 20 (`DEFAULT_SUBAGENT_MAX_CONCURRENT`), but deployments can
lower or raise it through `[subagents].max_concurrent` /
`SQUEEZY_SUBAGENT_MAX_CONCURRENT`. Even when the parent emits more delegate
calls in the same turn than the cap allows, only that many run at once:

```rust
// crates/squeezy-agent/src/lib.rs
    let cap = context.config.subagents.max_concurrent.max(1);
    let completions = futures_util::stream::iter(calls.into_iter().map(|(index, call, kind)| {
        let context = context.clone();
        async move {
            let outcome = run_subagent_dispatch(&context, &call, kind).await;
            (index, kind, outcome)
        }
    }))
    .buffer_unordered(cap)
    .collect::<Vec<_>>()
    .await;
```

The lease registry refuses overflow with a structured
`ConcurrencyCap` rejection — no exception, no model-side retry loop:

```rust
// crates/squeezy-agent/src/lib.rs:1035-1047
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        let active = state
            .values()
            .filter(|metadata| !metadata.cancel.is_cancelled())
            .count();
        let limit = max_concurrent.max(1);
        if active >= limit {
            return Err(SubagentStartError {
                reason: SubagentRejectionReason::ConcurrencyCap,
                limit,
                active,
            });
        }
```

`SubagentLease` is RAII: `drop(lease)` frees the slot. Dispatch drops
it the moment the child loop returns (`lib.rs:7380`).

### Tool subsetting

Dispatch computes the child's allowlist from a per-kind name set,
intersects it with the parent's `all_tool_specs`, and filters by
capability so only `Read` or `Search` tools survive:

```rust
// crates/squeezy-agent/src/lib.rs:8650-8660
    all_tool_specs
        .iter()
        .filter(|tool| names.contains(tool.spec.name.as_str()))
        .filter(|tool| {
            matches!(
                tool.capability,
                PermissionCapability::Read | PermissionCapability::Search
            )
        })
        .cloned()
        .collect()
}
```

The per-kind `names` set is one of
`DELEGATE_SUBAGENT_TOOL_NAMES`, `EXPLORE_SUBAGENT_TOOL_NAMES`,
`DOC_HELP_SUBAGENT_TOOL_NAMES`, or the `allowed_tools` from
`role_config(Planner|Reviewer)` (`lib.rs:8635-8649`).

Explorer resolves to twelve tools (`crates/squeezy-agent/src/roles.rs:54-67`);
Reviewer gets eight (lines 82–91). The child runs in forced
`SessionMode::Plan` so mode enforcement at the dispatcher refuses any
write-capable call:

```rust
// crates/squeezy-agent/src/lib.rs:7594-7597
    // Subagents in Plan mode are deliberately read-only; the active-plan
    // write exception applies to the top-level interactive session, not
    // to spawned subagents.
    let tool_specs = advertised_tool_specs(&allowed_tools, SessionMode::Plan, false);
```

The subsetting folds into the **lazy schema loading** mechanism. The
child starts with its own fresh `local_loaded_schemas`
(`lib.rs:7619`) — not the parent's loaded-schema state — so the
child's first model request advertises only the synthetic control
tools plus the configured `[tools].core` subset intersected with the
child's allowlist:

```rust
// crates/squeezy-agent/src/lib.rs:11671-11703
    if !schema_config.lazy_schema_loading {
        return advertised_tool_specs(tools, mode, plan_edit_allowed);
    }

    let mut specs = Vec::new();
    let mut seen = BTreeSet::new();
    // ...synthetic + core + loaded-schemas push, omitted for brevity...
    for name in loaded_tool_schemas {
        push_tool_spec_by_name(tools, name, mode, plan_edit_allowed, &mut specs, &mut seen);
    }
    specs
}
```

A child Explorer pays the per-round schema cost of, at worst, a dozen
read-only tools — versus the parent's full registry plus any
MCP-loaded tools.

### Result handoff

The child runs `run_subagent_loop` (`lib.rs:7624`) on a private
`conversation` (`lib.rs:7604`) seeded with one synthesized user
prompt. Its assistant/tool events route to a hidden mpsc channel a
background task drains so the parent's event stream never sees them:

```rust
// crates/squeezy-agent/src/lib.rs:7615-7616
    let (hidden_tx, mut hidden_rx) = mpsc::channel::<AgentEvent>(64);
    let drain_handle = tokio::spawn(async move { while hidden_rx.recv().await.is_some() {} });
```

What returns to the parent is a compacted summary bounded by
`max_summary_tokens` (default 64k tokens,
`crates/squeezy-core/src/lib.rs:257`) at four chars per token:

```rust
// crates/squeezy-agent/src/lib.rs:8085-8090
    let max_chars = (config.subagents.max_summary_tokens as usize)
        .saturating_mul(SUBAGENT_SUMMARY_CHARS_PER_TOKEN)
        .max(256);
    SubagentExecution {
        status: ToolStatus::Success,
        summary: compact_text(&summary, max_chars),
```

The parent sees only `summary`, `supporting_receipts`,
`files_touched`, `cost`, a `cache` breakdown, and aggregate
`metrics`. The raw sub-agent transcript is suppressed unless the
operator opts in via `subagents.include_transcript = true`
(`lib.rs:7660-7662`). Plan and Review kinds additionally parse a JSON
tail off the final assistant message into `structured_output` so the
parent can iterate findings as data without re-prompting
(`lib.rs:7653-7658`).

## Worked example

The parent model is debugging a regression. It emits three `explore`
calls in one turn — one per suspect subsystem (`auth`, `cache`,
`router`).

`flush_delegate_batch` receives all three. The lease registry hands out three
slots under the configured concurrent-subagent cap (20 by default). All three
children run concurrently. Each runs
`run_subagent` (`lib.rs:7540`), which: clones `AppConfig`
(`lib.rs:7545`) and forces `SessionMode::Plan`,
`store_responses = false`, inherited `max_output_tokens`, and per-call
ceilings from `config.subagents.max_tool_calls_per_call` /
`max_tool_bytes_read_per_call` / `max_search_files_per_call`
(`lib.rs:7564-7566`); resolves the model via
`subagent_model_for_kind` so an `Explore` child uses
`config.subagents.explore_model` or the per-provider cheap-tier
fallback (`lib.rs:8559-8563`); assembles the Explorer's twelve
read/search tools, dropping every write-capable tool; seeds a fresh
`conversation` with one user-text message built by
`subagent_user_prompt` (`lib.rs:8499`); runs until the model returns a
final assistant message (or `max_model_rounds`, default 1000, is hit);
returns the assistant text compacted to `max_summary_tokens * 4`
chars.

Approximate token math for three 12-tool Explorers, each taking
~8 model rounds and ending with a 400-word summary:

| component | inlined | sub-agent | parent saves |
| --- | --- | --- | --- |
| 3 × tool-call requests + results | ~30k tokens added to parent | stays in child | ~30k |
| 3 × intermediate assistant reasoning | ~6k | stays in child | ~6k |
| 3 × final summary | n/a (already part of inline trace) | ~0.6k each = 1.8k | -1.8k |

Net: one parent turn moves ~36k tokens into three isolated child
contexts and emits ~1.8k tokens of summary back. Every later parent
turn re-sends 1.8k, not 36k. With prompt caching on the parent
prefix, the parent's cache key is undisturbed by all that work.

Children run in parallel, so wall-clock cost is roughly one child's
runtime, not three. Provider input dollars per child are 12 tool
schemas × 8 rounds — small because lazy schema loading keeps each
child's per-round payload short.

## Edge cases & limits

### Step-budget exhaustion

`delegate_chain` validates `steps` before any lease is taken; a
request exceeding the cap returns a structured error the model can
read but cannot defeat:

```rust
// crates/squeezy-agent/src/lib.rs:8213-8215
    if steps_array.len() > DELEGATE_CHAIN_MAX_STEPS {
        return Err(format!(
            "delegate_chain `steps` may not exceed {DELEGATE_CHAIN_MAX_STEPS} steps, got {len}.",
```

The per-step model-rounds ceiling is separate: `max_model_rounds`
(default 1000, `crates/squeezy-core/src/lib.rs:241`) bounds LLM
rounds inside one child before `run_subagent_rounds` returns
`max_rounds_exceeded` (`lib.rs:8061-8076`) and the chain aborts
before launching the next step. Concurrency-cap rejection at
start-time is distinct; it bumps the global `subagent_failures`
counter but not the per-kind bucket (`lib.rs:7314-7339`).

### Tools a sub-agent can never have

The cleanest invariant is enforced by a test, not just by the
production filter — a sub-agent advertises no tool that would let it
spawn another sub-agent:

```rust
// crates/squeezy-agent/src/roles_tests.rs:43-53
    // Flat spawning invariant: subagents must never see delegate/explore/
    // delegate_plan/delegate_review in their advertised tool set, or one
    // subagent could spawn another and we'd lose the cost/cancellation
    // guarantees the parent depends on.
    //
    // Exact-name matching is not enough — a new spawn tool named
    // `delegate_research` or `review_subagent` would silently leak past a
    // literal-only allowlist while restoring hierarchical spawn. Reject by
    // exact name, by spawn-verb prefix family, and by any substring that
    // identifies the tool as subagent control surface.
```

The test then iterates every role's `allowed_tools` and rejects on
exact-name match against `CONTROL_TOOL_NAMES`, on
`SPAWN_TOOL_PREFIXES` (less `SPAWN_PREFIX_LEGITIMATE`), and on the
substring `"subagent"` anywhere in the tool name
(`roles_tests.rs:54-81`). Three layers of defense: per-kind
`DELEGATE_SUBAGENT_TOOL_NAMES` / role-catalog allowlist, runtime
capability filter that drops anything not `Read`/`Search`, and forced
`SessionMode::Plan`. Sub-agents are deliberately flat — one parent, N
children, no grandchildren. Cancellation, accounting, and budgets stay
linear in parent turns; a recursive spawn tree would break that.

### Cost trade-off

Each spawned child issues its own provider requests, so fixed
per-request overhead — TLS, JSON envelope, system prompt, core tool
schemas — is paid N times for N children. Inlining wins for a one-off
two-tool-call question; isolation wins when the inline version would
inflate every later parent turn or would consume the parent's
per-turn tool-call budget and prevent other work this turn.
`delegate` is for "wide and likely deep" research, not "grep one
thing".

### Model selection per sub-agent

```rust
// crates/squeezy-agent/src/lib.rs:8554-8567
    let policy = kind
        .role()
        .map(|role| role_config(role).model_policy)
        .unwrap_or(RoleModelPolicy::Parent);
    match (kind, policy) {
        (SubagentKind::Explore, _) => {
            config.subagents.explore_model.clone().unwrap_or_else(|| {
                cheap_model_for(provider, config).unwrap_or(parent_model.clone())
            })
        }
        (SubagentKind::DocHelp, _) => {
            cheap_model_for(provider, config).unwrap_or(parent_model.clone())
        }
        (_, RoleModelPolicy::Parent) => parent_model,
        (_, RoleModelPolicy::Cheap) => cheap_model_for(provider, config).unwrap_or(parent_model),
    }
```

Reviewer (`Cheap`, `roles.rs:125`) and Explorer (`Cheap`,
`roles.rs:103`) drop to the provider's cheap tier when one is
configured or curated. Planner stays on the parent model (`Parent`, `roles.rs:114`)
because planning quality suffers under cheap-tier. `Delegate` has no
role overlay and keeps the parent model; `DocHelp` uses the provider's cheap
tier when available and falls back to the parent model, with a separate output
budget floor so the user-visible answer can still be complete. There is no
"global cheap-model fast path" — cheap-tier fires only for the kinds
whose role catalog or subagent kind asks for it, and only when a cheap model
resolves (`cheap_model_for`, `lib.rs:8578-8589`). A child's
dollar savings come from two independent levers: smaller per-round
payload (tool subsetting + lazy schemas) and, for two of the four
kinds, a smaller per-token rate.

## Cost intuition

A would-be 30k-token inline expansion of the parent transcript
becomes 3 × N-token child contexts plus 3 × ≤0.5k-token summaries in
the parent. The child contexts are dropped at end-of-call — no
compaction needed, they were never in the parent's conversation
buffer. The summaries become permanent parent tokens, but at ≤0.5k
each they fit inside the parent's prompt-cache TTL and do not
invalidate the parent prefix.

Under the default concurrency cap (20) three children run simultaneously
and finish in roughly one child's wall time, not three. Tool
subsetting + lazy schemas means each child's request payload
advertises ~8–12 tool schemas instead of the parent's full registry
— proportional savings on every child round. Cheap-tier selection
for Reviewer and Explorer kinds lowers the per-token rate, multiplying
the per-round savings above.

The mechanism stacks with prompt caching (parent prefix is
untouched), with lazy schema loading (children pay reduced schema
costs per round), and with the compaction budget (the parent never
has to compact a 30k-token research detour it never ingested). The
parent's context window is a scarce, ever-growing resource; the
cheapest way to keep it small is to do the work somewhere else.
