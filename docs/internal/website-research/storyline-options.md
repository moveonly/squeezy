# Website Storyline Options

Repo-local research for future website work. This file is based on the current
checkout only; no network sources were used. It intentionally does not edit site
files.

## Scan Scope

Surfaces inspected:

- Product framing: `README.md`, `docs/THESIS.md`, `docs/internal/ARCHITECTURE.md`,
  `crates/squeezy-skills/external-docs/AGENT_APPROACH.md`.
- Website state: `squeezy-site/src/facts.ts`, `squeezy-site/src/pages/*.astro`,
  `squeezy-site/README.md`.
- Website research already present in this checkout:
  `docs/internal/website-research/features.md`,
  `cost-saving-methodology.md`, `cost-saving-data.md`, `languages.md`,
  `providers.md`, and `telemetry-posthog.md`.
- User-facing docs: `crates/squeezy-skills/external-docs/`.
- Internal evidence: semantic graph, benchmark, eval, validation, checkpoint,
  skills, subagent, telemetry, release, keybinding, and cost-saving docs under
  `docs/internal/`.
- Code surfaces: crate layout under `crates/`, graph tool specs/runtime,
  dispatch/slash-command handling, provider/auth/model registry notes, and
  benchmark/oracle directories.
- Operations surfaces: `.github/workflows/`, `scripts/`, `benchmarks/`, and
  `infra/telemetry-worker/`.

## Recommended Default

Use **Variant 1: Local Code Understanding Before Model Context** as the main
website story.

It is the cleanest fit for the implemented product: a Rust CLI/TUI coding agent
that builds local code structure, returns compact evidence packets, and uses the
model for judgment after deterministic navigation has narrowed the work. It also
avoids overclaiming: cost savings, speed, language coverage, permissions,
subagents, skills, and provider choice all become proof points under one
coherent premise rather than competing homepage theses.

Recommended homepage spine:

1. Hero: "Understand the repo before you ask the model."
2. Problem: agent cost usually comes from repeated code discovery.
3. Mechanism: local tree-sitter navigation, graph-backed tools, exact slices.
4. Proof: benchmark/cost chart with losses shown, language coverage, graph
   oracles.
5. Operating surface: terminal session, permissions, sessions, `/cost`,
   `/context`, local help.
6. Install: one-line installer, Homebrew, Cargo, releases.
7. Docs last in nav, with deeper implementation language there.

Keep front-page wording plain. Terms such as "semantic graph", "provenance",
"oracle", and "confidence label" can appear in diagrams, docs, and benchmark
pages, but the first viewport should say what the user gets: local code
understanding before paid model context.

## Storyline Variant 1: Local Code Understanding Before Model Context

### Shape

Squeezy is a coding agent that spends local CPU on repository understanding
before it spends model tokens. The website should feel like a practical tool for
large codebases, not a generic AI assistant.

### Homepage Promise

"Squeezy maps your code locally, then gives the model focused evidence instead
of raw file dumps."

Alternative hero lines:

- "Understand the repo before you ask the model."
- "Use local code structure first. Spend model context second."
- "A terminal coding agent built around code navigation, not repeated file
  skimming."

### Why This Works

- It directly matches the thesis in `docs/THESIS.md`: cost, speed, and code
  understanding are first-class rather than emergent.
- It matches the tool surface: `repo_map`, `decl_search`, `definition_search`,
  `reference_search`, `upstream_flow`, `downstream_flow`, `hierarchy`,
  `symbol_context`, `read_slice`, `diff_context`, and `plan_patch`.
- It lets language coverage and benchmark oracles become evidence rather than
  buzzwords.
- It leaves room for ordinary fallback tools. Unsupported languages still work
  through bounded list/grep/read flows, without graph confidence.

### Suggested Sections

- **The expensive part of an agent turn**: repeated grep/read loops, re-reading
  files after compaction, broad context loading.
- **Local structure first**: repository map, declaration/reference/call tools,
  exact source slices, diff context.
- **What the model sees**: paths, spans, hashes, confidence, freshness, and next
  actions instead of whole-file payloads.
- **Language coverage**: 13 language families / 17 source variants, with a clear
  caveat that coverage means graph-backed navigation for supported file types.
- **Benchmarked, not hand-waved**: cost scoreboard and graph oracle charts, with
  exact suite labels and limitations.
- **When graph evidence is not enough**: bounded grep/read, shell, compiler,
  web, and MCP tools behind permission policy.

### Best Pages Under This Variant

- `/how-it-works/`: explain the graph-first loop in plain language.
- `/languages/`: matrix by family, indexed facts, caveats, benchmark/oracle
  status.
- `/benchmarks/`: cost chart plus semantic graph accuracy/oracle evidence.
- `/tools/` or `/navigation/`: show the actual graph tools and fallback flow.
- `/install/`: make getting the binary easy.
- `/docs/`: technical docs last.

### Risks To Avoid

- Do not imply Squeezy replaces compilers, LSPs, or rust-analyzer.
- Do not claim complete code understanding for macros, dynamic dispatch,
  framework magic, reflection, generated code, or overload resolution.
- Do not hide unsupported/fallback behavior.
- Do not headline exact cost savings without the benchmark methodology beside
  the number.

## Storyline Variant 2: Every Token Has A Job

### Shape

Squeezy is for developers who care where model spend goes. The site leads with
cost observability and then explains the mechanisms: local graph navigation,
bounded outputs, prompt caching, lazy schemas, compaction, cheap routing, and
subagent isolation.

### Homepage Promise

"Spend model tokens on reasoning, not rediscovering your repository."

Alternative hero lines:

- "A coding agent with a visible token budget."
- "Keep repeated code discovery out of paid context."
- "Local evidence, bounded tools, and cost receipts for long coding sessions."

### Why This Works

- The repo has a strong cost-saving research surface:
  `docs/internal/cost-saving/*`, `docs/internal/website-research/cost-saving-*`,
  `/cost`, `/context`, prompt cache policy, micro-compaction, lazy tool schemas,
  and routing.
- The current public-ready benchmark says Squeezy used 20.3% less total model
  spend across the checked 15-language Mini benchmark while preserving measured
  recall, with 11 wins and 4 losses shown.
- It gives a concrete reason to care about local analysis beyond "faster":
  reducing repeated paid context.

### Suggested Sections

- **Where agent cost leaks**: repeated reads, tool-output bulk, broad searches,
  stale context after compaction.
- **Local evidence packets**: graph results and exact slices.
- **Context hygiene**: receipts, spill handles, micro-compaction, full
  compaction, pinned context.
- **Model selection**: conservative cheap-model routing, `/cheap`, `/parent`,
  `/router`, same-provider fallback.
- **Accounting you can inspect**: `/cost`, `/context`, cache counters when
  exposed, routing estimates, tool-call counts, subagent spend.
- **Measured results**: Mini cost chart, all rows shown.

### Best Pages Under This Variant

- `/cost/`: methodology, mechanisms, and charts.
- `/benchmarks/`: public-ready data with caveats.
- `/how-it-works/`: local evidence loop.
- `/providers/`: provider capabilities, prompt caching, pricing metadata caveats.
- `/docs/cost-receipts/`: deeper technical behavior.

### Risks To Avoid

- Do not say "always cheaper" or "guaranteed savings." Internal evidence shows
  near-parity and losing rows.
- Do not claim provider billing authority. USD estimates depend on available
  model/pricing metadata and provider usage fields.
- Do not imply prompt caching is universal.
- Do not make the homepage an accounting dashboard. The cost story is strongest
  when it stays attached to code navigation.

## Storyline Variant 3: A Local Agent You Can Inspect

### Shape

Squeezy is a local terminal agent for serious coding sessions: explicit
permissions, resumable logs, replay, checkpoints, bundled help, local skills,
MCP/web controls, and release/install surfaces. The site leads with trust and
operability instead of pure cost.

### Homepage Promise

"A local coding agent with inspectable sessions, reviewable tools, and bounded
external access."

Alternative hero lines:

- "Run the agent locally. Keep the session inspectable."
- "A terminal coding agent built for reviewable work."
- "Local sessions, explicit permissions, and code-navigation-first tools."

### Why This Works

- The repo has strong user-operation surfaces: session metadata/events/replay,
  `/sessions`, `/resume`, `/session-export`, `/report`, checkpoints, permission
  modes, shell sandbox planning, MCP server config, web guardrails, and local
  skills.
- It differentiates Squeezy from cloud-agent narratives without inventing a
  hosted product.
- It lets the TUI, config screen, keybindings, prompt queue, status line, and
  bundled docs feel like product features rather than implementation details.

### Suggested Sections

- **Local by default**: single Rust binary, CLI/TUI, no hosted service, no
  embedded HTTP server.
- **Reviewable actions**: capability policy for read/search/edit/shell/web/MCP,
  approval prompts, optional auto-review, shell sandbox caveats.
- **Long work survives restarts**: sessions, metadata, events, resume state,
  replay tape, export, labels, fork/resume.
- **Undo support**: optional checkpoints and turn-level reverts.
- **Controlled extension surfaces**: local filesystem skills, MCP servers,
  websearch/webfetch behind permissions.
- **Built-in help**: product-help questions answered from bundled local docs
  before provider work.

### Best Pages Under This Variant

- `/sessions/`: resume, replay, export, reports, redaction.
- `/permissions/`: approval policy, shell sandbox caveats, MCP/web.
- `/skills/`: local instruction bundles, not plugins or marketplace extensions.
- `/mcp-web/`: external tools and web evidence.
- `/install/`: local binary and release channels.

### Risks To Avoid

- Do not sell this as "secure sandboxing" without platform caveats.
- Do not imply persistent autonomous workers; subagents are short-lived and
  bounded.
- Do not call skills "plugins" unless the copy explicitly says they are local
  filesystem instruction bundles.
- Do not imply Squeezy is a hosted service, daemon, IDE plugin, SDK, or remote
  API.

## Navigation And Page Structure

Recommended product navigation:

1. **Home**: default story, local code understanding before model context.
2. **How It Works**: local graph, evidence packets, fallback tools, verification.
3. **Cost**: mechanisms plus benchmark methodology and honest charts.
4. **Languages**: supported families, indexed facts, benchmark/oracle status,
   limitations.
5. **Benchmarks**: Mini cost board, semantic graph accuracy, fixture/oracle
   matrix, methodology.
6. **Providers**: provider presets, auth paths, prompt-cache/cost caveats,
   local/custom routes.
7. **Install**: installer, Homebrew, Cargo, release artifacts, `doctor`.
8. **Roadmap**: specific gaps and next work, not aspirational market claims.
9. **Support**: troubleshooting, feedback/report, telemetry/privacy.
10. **Docs**: technical docs and command references last.

Optional deeper pages if the site grows:

- **Tools**: graph-navigation and fallback tools.
- **Sessions**: resume, replay, export, redaction, reports.
- **Permissions**: policy modes, shell sandbox caveats, MCP/web controls.
- **Skills**: local instruction bundles and scope guardrails.
- **MCP And Web**: configured external access with citations and validation.

Current site opportunity: `squeezy-site/src/facts.ts` still presents some
narrower summary numbers on the homepage than the current research supports
for languages/providers. Do not patch that here, but future site work should
refresh those numbers from the language/provider research files.

## Top Claims To Use

Each claim needs its caveat nearby on public pages.

| Claim | Strong public wording | Evidence | Caveat |
|---|---|---|---|
| Local-first code understanding | "Squeezy uses local code structure before asking the model to inspect source text." | `README.md`, `docs/THESIS.md`, `AGENT_APPROACH.md`, graph tool specs | Not every file type is graph-indexed. |
| Compact evidence packets | "Graph tools return paths, spans, hashes, confidence, freshness, and next actions." | `README.md`, `docs/internal/website-research/languages.md`, `crates/squeezy-tools/src/specs.rs` | Keep terms like provenance/confidence deeper than the hero. |
| Exact slices over whole files | "When structure is enough, Squeezy reads exact source slices instead of whole files." | `TOOLS.md`, `read_slice` spec/runtime, cost-saving docs | Raw reads still exist and are correct for unsupported/literal cases. |
| Language coverage | "Graph-backed navigation for Rust, Python, Java, Kotlin, Scala, C#/.NET, Go, C/C++, JavaScript/TypeScript, PHP, Ruby, Swift, and Dart." | `external-docs/LANGUAGES.md`, `website-research/languages.md` | Coverage is not compiler-perfect and varies by language. |
| Benchmark-backed cost result | "On the checked 15-language Mini benchmark, Squeezy used 20% less total model spend than the baseline while preserving measured recall; all losses are shown." | `website-research/cost-saving-data.md`, `mini-vs-codex-realworld.csv` | One suite, one setup, n=3 medians; do not generalize to all tasks. |
| Graph validation | "The benchmark suite validates graph output against language-specific oracles." | `docs/internal/BENCHMARKS.md`, `benchmarks/` | Oracles are benchmark aids, not production navigation dependencies. |
| Cost controls | "Squeezy keeps tool output bounded with receipts, spill handles, compaction, lazy schemas, and visible cost/context counters." | `cost-saving-methodology.md`, `cost-saving/*`, `SESSIONS.md` | Dollar estimates depend on provider metadata. |
| Provider flexibility | "Bring your own provider key across native, cloud-hosted, aggregator, local, and OpenAI-compatible routes." | `README.md`, `PROVIDERS.md`, `website-research/providers.md` | Feature parity and pricing metadata vary. |
| Reviewable operation | "File edits, shell, web, MCP, compiler, git, and destructive actions are capability-gated." | `features.md`, `APPROVAL_POLICY.md`, core permission code | Do not promise universal sandboxing, especially on Windows/best-effort modes. |
| Local session continuity | "Sessions can be resumed, exported, replayed, and reported from local logs." | `SESSIONS.md`, `EVAL_HARNESS.md`, store/session code | Logs are redacted and bounded; some data degrades by design. |
| Optional checkpoints | "When enabled, checkpoints help inspect and roll back agent edits." | `CHECKPOINTS.md`, feature research | Off by default and not a replacement for Git review. |
| Local skills | "Local skills keep reusable instructions near projects and load only when relevant." | `SKILLS_SCOPE.md`, `SKILLS.md`, skills crate | Not a marketplace or remote extension runtime. |
| Bounded subagents | "Short-lived subagents isolate exploration and return compact summaries." | `SUBAGENTS.md`, `AGENT_APPROACH.md`, roles code | Not autonomous fleets; child contexts still cost tokens. |
| Eval and validation | "Squeezy has deterministic validation and live-agent eval harnesses." | `VALIDATION_HARNESS.md`, `EVAL_HARNESS.md`, `crates/squeezy-harness`, `crates/squeezy-eval` | Live eval can spend provider tokens and is opt-in. |
| Installability | "Install with the shell installer, Homebrew, Cargo, or release archives." | `README.md`, `INSTALL.md`, release scripts | Keep release/platform matrix exact. |

## Sections To Avoid

Avoid these page concepts unless implementation changes first:

- **"Autonomous agent fleet"**: subagents are short-lived, bounded, and mostly
  read/search/navigation scoped.
- **"Compiler-perfect understanding"**: the graph is local tree-sitter plus
  deterministic heuristics, with benchmark oracles outside production.
- **"Secure sandbox"**: platform behavior varies; Windows lacks filesystem and
  network isolation, and best-effort modes can degrade.
- **"Universal language support"**: unsupported languages fall back to bounded
  search/read, not graph confidence.
- **"Guaranteed cheaper than other agents"**: benchmark rows include parity and
  losses.
- **"Hosted Squeezy"**: current architecture is CLI/TUI, no app-server, daemon,
  SDK, remote API, or embedded HTTP server.
- **"Plugin marketplace"**: skills are local filesystem instruction bundles.
- **"Billing-grade analytics"**: cost accounting is provider-reported or
  estimated from local metadata.
- **"Invisible telemetry"**: website/product telemetry must be described with
  opt-out/privacy details when discussed.
- **"All providers, same features"**: provider auth, streaming, prompt caching,
  document attachment support, and pricing metadata differ.
- **"AI reviewer replaces the user"**: auto-review is opt-in and bounded; high
  risk/destructive requests still route to human approval or denial.

## Broad Subject Inventory

### Core Product Story

- Cost-aware coding agent TUI with local semantic navigation.
- Rust implementation and single-binary CLI/TUI.
- Local-first thesis: deterministic repository analysis before model calls.
- Graph-first agent instructions baked into defaults.
- Plan mode vs build mode: mutation hidden in plan mode, available behind
  permissions in build mode.
- Local help first: Squeezy product questions answered from bundled docs before
  provider work.

### Code Navigation

- `repo_map`: architecture map, language counts, coverage, unsupported files,
  next actions.
- `decl_search` and `definition_search`: declaration and likely definition
  lookup.
- `reference_search`: symbol-bound or heuristic references.
- `upstream_flow` and `downstream_flow`: callers, callees, references, bounded
  call-chain context.
- `hierarchy`: containment hierarchy.
- `symbol_context`: compact context around matching symbols, callers, callees,
  references, dirty/diff annotations.
- `read_slice`: exact source slice by symbol, line, byte range, path, or diff
  mode.
- `diff_context`: current Git changes plus compact semantic cross-references.
- `plan_patch`: patch planning from graph impact context.
- Bounded `grep`, `glob`, `read_file` as fallback for literals, unsupported
  files, and ordinary text search.

### Language And Benchmark Story

- 13 supported language families / 17 source variants:
  Rust, Python, Java, Kotlin, Scala, C#/.NET, Go, C/C++,
  JavaScript/TypeScript, PHP, Ruby, Swift, Dart.
- Indexed facts vary by language: declarations, imports, references, calls,
  containment, inheritance/mixins/implements, project facts where implemented.
- Unsupported files are explicitly fallback evidence.
- Benchmark fixtures and smoke specs exist per language family.
- Full-tier corpora cover many families; Dart and some newer rows require
  careful caveats from current docs.
- Oracle inventory: rust-analyzer, CPython AST, javac, Kotlin compiler PSI,
  Scala SemanticDB, Roslyn, Go parser/types, clang, TypeScript compiler API,
  nikic/PHP-Parser, Ruby Prism, SourceKit-LSP, Dart analyzer.
- Mixed workload support currently strongest for Rust, C#, Go, C/C++,
  JavaScript/TypeScript, and PHP.
- Public-ready data includes Mini real-world cost board, Rust declaration
  accuracy, Java declaration accuracy, Go declaration accuracy, and selected
  smoke-fixture baselines.

### Cost And Context Controls

- Semantic graph and evidence packets reduce broad reads.
- Tool output shaping: caps, truncation, aggregate budgets, spill handles.
- Receipt stubs for repeated reads/tool outputs.
- Prompt caching support in provider adapters where available.
- Lazy tool schema loading and compact tool indexes.
- Lazy skill body loading through `load_skill`.
- Conversation compaction and micro-compaction.
- Cheap-model routing and same-provider fallback.
- Subagent isolation of exploration/review/doc-help work.
- `/cost` and `/context` surfaces for token, cache, tool, subagent, routing,
  read, spill, receipt, and estimate visibility.
- Costly provider checks are explicitly gated and ignored by default.

### Safety, Permissions, And Reviewability

- Capability-scoped permission policy: read, search, edit, shell, web/network,
  MCP, git, compiler, destructive.
- Permission modes and presets: default, auto-review, full-access, custom.
- Optional AI reviewer for bounded permission review, not a user replacement.
- Shell sandbox planning and platform caveats.
- Protected metadata directories and stale-content checks for writes.
- Web tools with citation/cache receipts, textual content limits, DNS/internal
  address protections, and cross-origin redirect handling.
- MCP server configuration with transports, allow/deny lists, auth env vars,
  timeouts, enable/disable/restart/refresh.
- MCP elicitation audit/permission behavior.
- Optional checkpoints: list, show, undo, revert turn.

### TUI And Workflow

- First-run setup picker: theme, provider, key, model, reasoning effort.
- `/config` and F11 configuration screen with User/Repo/Local scopes.
- Settings watcher for reloadable config changes.
- Prompt queue overlay: submit/paste during active turns, select/reorder/delete.
- User-editable keybindings for auxiliary actions.
- Slash commands: help, config, MCP, model/permissions, plan/build, cost,
  context, reviewer, attachments, compact, pins, diff, tasks, feedback/report,
  sessions, resume/fork/export, checkpoints, effort/verbosity, status line,
  theme/spinner/keymap, routing.
- Status line and transcript surfaces for cost/context visibility.
- Local product docs embedded through `squeezy-skills`.

### Sessions, Replay, And Reports

- Local session directory with `metadata.json`, `events.jsonl`,
  `resume_state.json`, attachments, and `replay.jsonl`.
- Session discovery by branch/status/query.
- Resume, show, replay, export, report, cleanup, label/rename/fork surfaces.
- Redaction before persistence.
- Replay detects request/tool hash divergence.
- `/report` and feedback routes for consented maintainer intake.
- Session cost and context counters survive resume.

### Providers And Models

- Native providers: OpenAI, Anthropic, Google Gemini, Azure OpenAI, Bedrock,
  Ollama.
- Subscription/OAuth-oriented surfaces are present for OpenAI Codex/ChatGPT and
  GitHub Copilot in provider research, but public copy should stay conservative
  where selectable/runtime support or pricing metadata differs.
- Aggregators/gateways: OpenRouter, Vercel AI Gateway, PortKey.
- OpenAI-compatible presets: Groq, xAI, DeepSeek, Mistral, Together, Fireworks,
  Cerebras, DeepInfra, Baseten, Cloudflare Workers AI, Cloudflare AI Gateway,
  LM Studio, vLLM, llama.cpp, custom `openai_compatible`.
- Cloud IAM routes: Bedrock and Vertex-style OAuth behavior.
- Provider registry includes model capabilities, context estimates, prompt-cache
  support, and pricing where available.
- Missing pricing means token accounting can continue while dollar estimates are
  unavailable.

### Installation, Release, And Operations

- One-line installer for macOS/Linux.
- Homebrew formula support.
- Cargo install support.
- Release archives for macOS Intel, macOS Apple Silicon, Linux x86_64 musl, and
  Windows x86_64.
- `squeezy doctor` reports config/provider/session/sandbox health.
- Release workflow enforces tag/version matching.
- Local release smoke script exercises Cargo dry-run, install.sh, Homebrew, and
  winget generator paths without publishing.
- Dependabot coverage for Cargo, benchmark harness dependencies, and GitHub
  Actions.

### Evaluation And CI

- Deterministic validation harness (`squeezy-harness`) for CI without API keys.
- Live-agent eval harness (`squeezy-eval`) for scenario-driven QA, traces,
  frames, findings, tickets, and diffs.
- Offline mock provider scenarios.
- Real-world graph-vs-no-graph benchmark scenarios in eval fixtures.
- Benchmark workflow with one job per language family and reusable setup.
- Language doc checker script.
- Test-layout checker script.
- Clean-env wrapper for non-costly provider test safety.

### Telemetry, Feedback, And Privacy

- Product telemetry worker routes for batch telemetry, feedback, report archive,
  and website events.
- Website events go through `/v1/site` worker proxy, not browser-to-PostHog
  direct JavaScript.
- Website telemetry respects DNT/GPC/local opt-out in current site code.
- Worker validates/caps site event bodies and forwards sanitized PostHog batch
  events when configured.
- Dashboard setup script covers product, reliability/runtime, website, feedback,
  and reports.
- Public website copy should keep telemetry opt-out and sanitized-event details
  near any analytics mention.

### Implementation Texture Worth Showing Carefully

- 18-crate Rust workspace with clear boundaries.
- `redb` storage direction and persisted graph/cache state.
- Tree-sitter as production parser layer.
- Compiler/LSP/oracle tools used for benchmarks and explicit verification, not
  normal navigation.
- Workspace discovery, ignore handling, and fallback/exclusion reporting.
- VCS/checkpoint support through local state, not a remote service.
- No hosted service, no app server, no daemon, no inbound HTTP listener, no SDK
  client crate.

## Copy Guardrails

Use:

- "local code structure", "graph-backed navigation", "focused evidence",
  "bounded tools", "reviewable actions", "local sessions", "provider choice",
  "measured benchmark suite".

Avoid:

- "compiler-perfect", "universal", "autonomous", "secure by default",
  "guaranteed savings", "zero-cost", "marketplace", "cloud agent", "IDE",
  "server", "SDK".

Good short formulation:

> Squeezy is a local terminal coding agent that maps supported code with
> tree-sitter, answers navigation questions from that structure, and sends the
> model compact evidence instead of defaulting to whole-file context.

Good benchmark formulation:

> On the checked 15-language Mini benchmark, Squeezy's graph-enabled agent used
> 20% less total model spend than the baseline while preserving measured recall.
> The chart should show all rows, including the four losses.

Good caveat line:

> Graph-backed coverage is local static analysis, not compiler-perfect runtime
> understanding. Unsupported files still work through bounded search and reads
> without graph confidence.
