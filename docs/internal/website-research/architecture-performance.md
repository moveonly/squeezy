# Website Architecture And Performance Research

This note inventories implemented architecture, performance, durability, and
repo-scale engineering facts that can support website copy. It is based on local
repo sources only; no network sources were used.

Keep public positioning concrete and modest. The strongest story is that Squeezy
spends local Rust code on deterministic repository understanding before it spends
model context, and that the implementation has explicit guardrails for latency,
durability, and benchmark honesty.

## Positioning Summary

Public framing:

- Squeezy is a Rust CLI/TUI coding agent built around local semantic navigation,
  not a web app, daemon, IDE plugin, or language-server wrapper.
- The repo is organized into focused Rust crates for CLI startup, TUI,
  orchestration, tools, parsers, graph state, ranking, persistence, telemetry,
  sessions, VCS/checkpoints, and benchmarks.
- The semantic graph is tree-sitter-backed, confidence-labelled, refreshable,
  and benchmarked against compiler or language-service oracles where those
  oracles are useful.
- Performance work is visible in the architecture: startup milestones, deferred
  graph opening, bounded graph-tool wait, per-query refresh budgets, parallel
  parsing for larger batches, batched redb writes, and graph build/refresh
  reports.
- Durability is local: session logs, repo profiles, receipt/read snapshots,
  compaction checkpoints, graph partitions, and cache metadata live on disk.

Use plain language first. "Understands code locally before asking the model" is
better for a home page than "semantic graph." Save tree-sitter, confidence
labels, redb, and oracle benchmarks for deeper technical sections.

## Claims We Can Make

| Area | Claim | Website-safe wording | Caveats | Source references |
| --- | --- | --- | --- | --- |
| Rust architecture | Squeezy is a Rust workspace with focused crates for the CLI, TUI, agent orchestration, tools, parser runtime, semantic graph, ranking, local store, VCS/checkpoints, telemetry, and validation. The root workspace currently lists 18 members. | "A Rust-first terminal agent with separate crates for the interface, agent loop, tools, parsers, graph, persistence, and validation." | Do not imply a public SDK or server architecture. Internal docs explicitly call Squeezy a single-process CLI/TUI and reject server/client/protocol/daemon expansion. | `Cargo.toml:1-21`, `docs/internal/ARCHITECTURE.md:8-31`, `docs/internal/ARCHITECTURE.md:126-149` |
| CLI/TUI shape | The first interface is the TUI, while one-shot prompt and subcommand paths go through the same CLI binary. Startup loads layered config, resolves sessions/resume, builds the provider, then enters the TUI or prompt path. | "A local terminal workflow for repeated coding sessions, plus one-shot CLI use when scripting is enough." | Avoid "IDE" or "web dashboard" language. Squeezy is intentionally not an app server. | `docs/internal/ARCHITECTURE.md:3-6`, `docs/internal/ARCHITECTURE.md:33-50`, `crates/squeezy-cli/src/main.rs:591-810` |
| Local-first navigation | Squeezy uses tree-sitter parsers, workspace file records, and local graph heuristics to answer navigation questions before asking the model to read raw files. | "Find definitions, references, callers, and focused code slices locally before spending model context." | Do not call it compiler-perfect or LSP-equivalent. The graph carries confidence labels and documented fallback paths. | `docs/internal/SEMANTIC_GRAPH.md:3-17`, `docs/internal/SEMANTIC_GRAPH.md:96-120`, `docs/internal/SEMANTIC_GRAPH.md:203-235` |
| Multi-language parser runtime | The parser runtime supports tree-sitter-backed parsing for C, C#, C++, Dart, Go, Java, JavaScript/JSX, Kotlin, PHP, Python, Ruby, Rust, Scala, Swift, TypeScript/TSX, with unsupported inputs represented as structured fallbacks. | "Tree-sitter-backed navigation across the languages Squeezy indexes, with explicit fallback for unsupported files." | Language precision varies. Keep per-language promises aligned with `docs/external/LANGUAGES.md`; do not say every language has identical call/reference accuracy. | `Cargo.toml:87-113`, `crates/squeezy-parse/src/lib.rs:353-383`, `docs/internal/SEMANTIC_GRAPH.md:49-61` |
| Parser performance | Parse batches of at least eight files run in parallel using available CPU parallelism. Each worker owns its own parser pool; outputs are sorted back into deterministic order. Cached trees are reused when hash/language match, and changed ranges use tree-sitter incremental parse when prior source exists. | "Large parse batches use CPU parallelism while small edits stay cheap and deterministic." | Do not claim every refresh is parallel. Small batches intentionally stay serial to avoid thread setup overhead. | `crates/squeezy-parse/src/lib.rs:267-350`, `crates/squeezy-parse/src/lib.rs:385-442`, `docs/internal/SEMANTIC_GRAPH.md:170-174` |
| Graph refresh latency | Graph refresh has debounce, idle interval, and per-tool refresh budget defaults. It compares stable hashes, reparses changed files only, removes deleted files, and records whether a refresh exhausted its budget. | "Graph tools refresh changed files under a predictable latency budget instead of re-indexing the repo on every query." | Budget exhaustion means a tail of dirty files can settle over later tool calls. Avoid "instant freshness after every bulk edit." | `docs/internal/SEMANTIC_GRAPH.md:122-154`, `crates/squeezy-graph/src/lib.rs:2207-2482` |
| Watcher support | `squeezy-graph` includes a cross-platform file watcher using OS-native backends through `notify-debouncer-full`; it groups file events into debounced batches and feeds `pending_changed_paths`. `GraphManager::open_watching` attaches it for long-lived processes, while one-shot callers can skip the startup cost. | "Long-lived sessions can receive debounced filesystem events, while one-shot paths avoid watcher overhead." | The current semantic graph doc still describes the default policy as tool-event-first with polling fallback and no always-on watcher. Do not claim the watcher is always active in every session unless the runtime path is verified. | `crates/squeezy-graph/src/watcher.rs:1-21`, `crates/squeezy-graph/src/watcher.rs:80-117`, `crates/squeezy-graph/src/lib.rs:2031-2055`, `docs/internal/SEMANTIC_GRAPH.md:134-139` |
| Startup performance | Startup has opt-in trace milestones, with in-memory timings available for telemetry even when no trace file is configured. The CLI marks phases such as main start, CLI parse, config load, model selection, repo profile, telemetry, session resolution, update banner, and provider build. | "Startup is instrumented so time-to-interactive regressions can be measured by phase." | Avoid publishing specific startup times without a current measured run and hardware/context. | `crates/squeezy-core/src/startup_trace.rs:1-10`, `crates/squeezy-core/src/startup_trace.rs:33-65`, `crates/squeezy-cli/src/main.rs:536-546`, `crates/squeezy-cli/src/main.rs:591-669`, `crates/squeezy-cli/src/main.rs:724-783` |
| Deferred graph opening | Tool registry construction defers `GraphManager::open_with_store` onto the blocking pool when an async runtime is available because graph open walks the workspace, initializes grammars, and hydrates redb partitions. First graph tool calls wait up to a bounded graph-ready timeout before falling back. | "The TUI can become interactive while the repository graph opens in the background; the first graph query waits only within a configured bound." | Do not say the graph is always ready before the first prompt. Pathological or very large workspaces can hit the fallback. | `crates/squeezy-tools/src/lib.rs:169-192`, `crates/squeezy-tools/src/lib.rs:1348-1396` |
| Local persistence | `squeezy-store` separates local state into repo profiles, session metadata/event logs, `state.redb` for receipts/read snapshots/observations/session-side cache, and `graph.redb` for semantic graph partitions and resolver-cache snapshots. | "Durable local state keeps sessions, receipts, snapshots, and graph cache data on disk without a cloud service." | Older docs may still refer to graph partitions under `state.redb`; current store code names a separate `graph.redb`. Use the split-cache wording. | `crates/squeezy-store/src/lib.rs:1-13`, `crates/squeezy-store/src/lib.rs:38-44` |
| redb durability and migration | The state store backs up and reinitializes on schema mismatch, fast-rotates oversized `state.redb`, and batches graph partition writes to avoid per-file fsync cost. Graph partition warm-start loads only metadata-matching, hash-matching records; mismatches rebuild. | "Local caches are versioned, backed up on schema mismatch, and written in batches so persistence does not dominate graph builds." | Do not call redb storage a synchronization layer or cloud memory. It is local durability/cache state. | `crates/squeezy-store/src/lib.rs:94-145`, `crates/squeezy-store/src/lib.rs:176-185`, `crates/squeezy-graph/src/lib.rs:2074-2086`, `crates/squeezy-graph/src/lib.rs:2493-2546` |
| Resolver-cache groundwork | Store tables and graph code can write per-file resolver snapshots and import adjacency snapshots, and scheduler types model SCC/topological levels for a phased resolver. | "The graph stack is being structured for deterministic cross-file resolver caches and import-cycle handling." | Avoid saying warm-start resolver-cache read reuse is fully shipped. `resolver_cache.rs` and scheduler comments say read-side / consumer wiring is still future work. | `crates/squeezy-store/src/lib.rs:59-71`, `crates/squeezy-graph/src/resolver_cache.rs:1-7`, `crates/squeezy-graph/src/resolver_cache.rs:36-50`, `crates/squeezy-graph/src/cross_file/scheduler.rs:1-12`, `crates/squeezy-graph/src/cross_file/scheduler.rs:83-101` |
| Benchmark discipline | Semantic graph benchmarks use language-specific oracles as testing aids while production navigation stays tree-sitter/local-graph. CI fails on missing expected results, graph+query slower than validation when an oracle is available, or refresh reparsing more files than edited. Mixed workloads generate scenarios from indexed symbols and resolved call edges. | "Benchmarks compare Squeezy's local graph against language oracles where useful, while keeping those oracles out of the runtime path." | Do not overgeneralize from dated local benchmark tables. Publish dates, fixtures, and caveats if numbers are used. | `docs/internal/BENCHMARKS.md:1-14`, `docs/internal/BENCHMARKS.md:67-113`, `docs/internal/BENCHMARKS.md:115-150` |
| Performance data worth promoting carefully | Internal docs record Rust mixed-workload runs, Java/C# local release runs, and graph retrieval cost wins. They also document known failures and cost-loss cases. | "Squeezy tracks accuracy, latency, fallback quality, and cost together, and records losses instead of hiding them." | Avoid fixed broad claims like "always faster" or "always cheaper." If using numbers, date them and include the benchmark target and caveats. | `docs/internal/BENCHMARKS.md:177-228`, `docs/internal/BENCHMARKS.md:230-257`, `docs/internal/cost-saving/13-graph-retrieval-in-practice.md:5-18`, `docs/internal/cost-saving/13-graph-retrieval-in-practice.md:54-68` |
| Memory scope | Cross-session user memory is a single static file read once at session start and capped by config. There is no model-writable memory tool in the v1 graph milestone. | "Durable user memory stays local and user-curated." | Do not market automated long-term memory, background extraction, per-thread memory, or agent-written memory until implemented. | `docs/internal/MEMORY_SCOPE.md:1-17`, `docs/internal/MEMORY_SCOPE.md:19-43` |

## Claims To Avoid

- Avoid "compiler-perfect", "full semantic understanding", "LSP-grade", or
  "rust-analyzer replacement." Production navigation is tree-sitter/local graph;
  compiler and language-service tools are benchmark oracles or explicit fact
  refreshes, not the live navigation dependency.
- Avoid "always faster" or "always cheaper." Internal cost docs explicitly say
  the graph pays off for cross-file tasks and can lose on small single-file
  tasks.
- Avoid fixed savings percentages unless a dated methodology, provider prices,
  task mix, baselines, and result tables ship next to the claim.
- Avoid saying the graph is always hot before the first prompt. The graph opens
  in the background and the first graph tool call has a bounded wait/fallback.
- Avoid saying every language has the same precision or coverage. The parser
  runtime covers many languages, but benchmarks and limitations differ by
  family.
- Avoid implying graph refresh is a background stream of partial truth. The
  refresh budget deliberately yields and reports `budget_exhausted` instead of
  serving stale evidence as fresh.
- Avoid claiming the watcher is always-on globally. The code supports an
  `open_watching` constructor and a watcher module; the current graph policy
  still documents tool-event-first refresh plus polling fallback.
- Avoid saying resolver-cache warm-start fully reuses all cross-file resolver
  work. The store tables and write path exist, but comments still identify
  read-side consumption and scheduler integration as follow-up work.
- Avoid saying redb is cloud sync, team memory, or durable semantic truth. It is
  local cache/state with schema-versioned rebuild paths.
- Avoid "AI remembers your project automatically." Current memory is a single
  user-curated local file read at startup; model-written memory is deferred.
- Avoid "secure sandbox" in architecture/performance sections unless the copy
  includes platform and permission caveats from the feature research note.
- Avoid public copy that centers implementation jargon on the home page. Use
  plain value language first; link to a technical page for graph, redb, and
  benchmarks.

## Website Section Ideas

### 1. Local Code Understanding

Goal: explain the architecture without jargon.

Possible copy:

> Squeezy reads your repository with local parsers first. It builds a compact
> map of definitions, references, callers, and code slices before asking the
> model to reason over raw files.

Supporting bullets:

- Tree-sitter-backed parser runtime for the supported source families.
- Confidence-labelled graph answers, with structured fallback for unsupported
  or low-confidence files.
- Focused slices instead of whole-file context by default.

Good visual idea: a simple flow from "repo files" to "parser runtime" to
"semantic graph" to "focused model context"; keep it factual, not decorative.

### 2. Built For Repo-Scale Work

Goal: promote engineering that matters on larger repositories.

Possible copy:

> Repository indexing is treated as a real runtime path: large parse batches use
> CPU parallelism, graph refreshes reparse changed files only, and each graph
> query has a refresh budget so one branch switch does not strand the session.

Supporting bullets:

- Parallel parse threshold for larger batches.
- Stable hashes and persisted graph partitions.
- Refresh reports expose reparsed files, bytes, event-vs-poll changes, language
  counts, and budget exhaustion.

### 3. Fast First Paint, Bounded First Graph Query

Goal: explain the startup trade-off plainly.

Possible copy:

> Squeezy does not make the terminal wait for a full repository graph before the
> interface can start. The graph opens in the background, and graph tools use a
> bounded first-query wait before falling back.

Supporting bullets:

- Startup trace milestones for time-to-interactive debugging.
- Deferred graph open through the blocking pool.
- Configurable `SQUEEZY_GRAPH_READY_WAIT_MS` for unusual workspaces.

### 4. Local Durability

Goal: make local-first durability concrete without overpromising memory.

Possible copy:

> Sessions, receipts, read snapshots, compaction checkpoints, and graph cache
> partitions are stored locally. Caches are versioned and rebuilt when metadata
> no longer matches the workspace.

Supporting bullets:

- `state.redb` for session-side cache state and receipts.
- `graph.redb` for semantic graph partitions and resolver-cache snapshots.
- Schema mismatch backup/rebuild path.
- Single user-curated memory file; no agent-written memory pipeline yet.

### 5. Benchmarks With Oracles

Goal: turn benchmark rigor into trust.

Possible copy:

> Benchmarks use slower compiler or language-service checks as oracles, but
> those tools stay out of the production navigation path. Reports track accuracy,
> latency, fallback quality, and refresh behavior.

Supporting bullets:

- Smoke fixtures and pinned corpus manifest.
- Rust rust-analyzer probes, Python `ast`, Java JDK tree API, C/C++ clang,
  C# Roslyn, JS/TS TypeScript service where available.
- Results should be dated and caveated when exposed publicly.

### 6. Engineering Guardrails

Goal: show the repo is intentionally scoped.

Possible copy:

> The architecture stays small enough to reason about: one Rust binary, no
> background daemon, no embedded HTTP server, no proprietary SDK surface, and
> extension through local tools/MCP rather than a Squeezy-specific remote
> protocol.

Supporting bullets:

- Single-process CLI/TUI non-goals.
- Crate boundaries are explicit.
- Release binary hygiene strips public artifacts.
- Internal docs separate contributor decisions from bundled user help.

## Suggested Technical Page Outline

1. "Local first, model second"
   - Explain graph-first navigation and focused evidence packets.
2. "How the graph stays responsive"
   - Parallel parse batches, hash comparison, refresh budgets, deferred graph
     open, watcher support.
3. "What gets persisted locally"
   - Sessions, receipts, snapshots, graph partitions, schema versioning.
4. "How Squeezy is measured"
   - Benchmark corpus, oracles, mixed workloads, limitations, dated results.
5. "Where we are careful"
   - Language-specific limits, no LSP runtime dependency, no automatic memory,
     no fixed savings promises.

## Short Public Phrases

- "Understand the code locally before spending model context."
- "A Rust terminal agent with local semantic navigation at the center."
- "Definitions, references, callers, and focused code slices without starting a
  language server."
- "Graph refresh is budgeted, measured, and honest about stale work."
- "Local redb caches make repeated sessions cheaper to reopen without turning
  Squeezy into a daemon."
- "Benchmarked against language oracles, but not dependent on them at runtime."

## Evidence Reviewed

- `Cargo.toml`
- `docs/internal/ARCHITECTURE.md`
- `docs/internal/SEMANTIC_GRAPH.md`
- `docs/internal/MEMORY_SCOPE.md`
- `docs/internal/BENCHMARKS.md`
- `docs/internal/cost-saving/13-graph-retrieval-in-practice.md`
- `docs/internal/website-research/features.md`
- `docs/internal/website-research/cost-saving-methodology.md`
- `crates/squeezy-cli/src/main.rs`
- `crates/squeezy-core/src/startup_trace.rs`
- `crates/squeezy-parse/src/lib.rs`
- `crates/squeezy-graph/src/lib.rs`
- `crates/squeezy-graph/src/watcher.rs`
- `crates/squeezy-graph/src/resolver_cache.rs`
- `crates/squeezy-graph/src/cross_file.rs`
- `crates/squeezy-graph/src/cross_file/scheduler.rs`
- `crates/squeezy-store/src/lib.rs`
- `crates/squeezy-tools/src/lib.rs`
- `benchmarks/README.md`
