# Architecture

Squeezy is implemented in Rust and targets local, deterministic code assistance
on macOS and Linux. The TUI is the first interface. Navigation is built on
tree-sitter and Squeezy's own semantic graph rather than LSP or
`rust-analyzer`.

## Crate Boundaries

- `squeezy` (`crates/squeezy-cli/`): command-line entrypoint, config
  initialization/inspection, provider startup, repo/session/feedback/MCP
  subcommands, and health checks.
- `squeezy-tui`: terminal UI, slash commands, approval prompts, status lines,
  local command handlers, and transcript rendering.
- `squeezy-agent`: turn orchestration, mode gating, help interception, lazy tool
  schema loading, context compaction, job execution, telemetry emission, and
  provider request shaping.
- `squeezy-core`: typed configuration, settings merge precedence, permissions,
  redaction, budgets, session mode, tool schema config, graph/cache settings,
  and user/project settings templates.
- `squeezy-llm`: provider abstraction plus OpenAI, Anthropic, Google, Azure
  OpenAI, Ollama, and Bedrock provider metadata/adapters.
- `squeezy-tools`: first-party tool specs and runtimes, checkpoints, shell
  sandbox integration, web tools, graph/search/read/edit tools, and MCP tool
  wrapping.
- `squeezy-skills`: local `SKILL.md` discovery/loading and built-in Squeezy
  help.
- `squeezy-workspace`, `squeezy-parse`, `squeezy-graph`, `squeezy-rank`,
  `squeezy-store`, `squeezy-vcs`, `squeezy-telemetry`, and `squeezy-harness`:
  workspace discovery, parsers, graph state, ranking helpers, persistent local
  state, VCS/checkpoint support, anonymous telemetry, and validation tasks.

## Runtime Flow

The CLI loads layered settings into `AppConfig`, applies CLI overrides, prints
health or config output when requested, then starts either a non-interactive
prompt or the TUI. The agent is constructed with the selected provider and
current config.

For each user turn, the agent first checks whether the input is Squeezy product
help. If so, `SqueezyHelp` returns a local answer from embedded external docs
and redacted config inspection without sending a provider request. Otherwise the
agent builds a provider request with mode-appropriate tools, optional lazy tool
schema indexes, current context attachments, session history, and compact
runtime instructions.

Tool calls are executed locally through `squeezy-tools`, with permissions and
mode checks before runtime dispatch. Mutating tools create checkpoints. Tool
results are redacted, cost-accounted, optionally spilled behind receipts, and
fed back to the provider loop.

## Documentation Boundary

User-facing behavior belongs in `docs/external/` because those files are bundled
into in-product help. Contributor workflow, implementation decisions, benchmark
oracles, release/deployment notes, and maintenance conventions belong in
`docs/internal/`.

When moving an external doc, update the help topic citations and the embedded
doc list in `crates/squeezy-skills/src/help.rs`. Tests should fail if a topic
cites a missing doc or if an internal doc is accidentally bundled into normal
help.

## Provider SDK Policy

An earlier rule of thumb said Squeezy should not depend on any vendor SDK
and should call provider APIs directly with `reqwest`. That guidance is
retired:

- **Vendor SDKs are allowed when they materially reduce auth, retry, or
  pagination complexity.** Bedrock is the existing example — SigV4 is
  not practical to reimplement, so `aws-sdk-bedrockruntime` and
  `aws-config` are the right tools.
- **Raw `reqwest` remains the default for simple bearer-token REST
  APIs** (Anthropic, OpenAI, Google, Azure OpenAI, Ollama). This is
  momentum, not principle. Do not rewrite an existing provider purely
  to add an SDK; only reach for one when the new provider's auth or
  protocol justifies it.
- **No embedded HTTP server, ever.** Squeezy is CLI/TUI only. Do not
  add `axum`, `warp`, `actix-web`, or any other framework that accepts
  inbound connections. The current `squeezy ask` Unix-socket bridge is
  the only acceptable form of local IPC, and it is bound to a session
  socket — not a network port.

## Adding A Language Family

1. Add the `LanguageKind` variant and map it to a `LanguageFamily` in
   `squeezy-core`.
2. Add extensions to `LanguageFamily::file_extensions`.
3. Register a `LanguageBackend` in `squeezy-parse`.
4. Register a `LanguageGraphExt` in `squeezy-graph`.
5. Register a benchmark `LanguageOracle` in `benchmarks/squeezy-graph-bench`.
6. Add fixture and spec files under `benchmarks/fixtures/` and
   `benchmarks/specs/`.
7. Add the language family to the benchmark workflow and update
   `docs/external/LANGUAGES.md` with user-facing coverage and limitations.
