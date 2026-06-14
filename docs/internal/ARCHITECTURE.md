# Architecture

Squeezy is implemented in Rust and targets local, deterministic code assistance
on macOS, Linux, and Windows. The TUI is the first interface, with
non-interactive CLI prompts and subcommands sharing the same core runtime.
Navigation is built on tree-sitter and Squeezy's own semantic graph rather than
LSP or `rust-analyzer`.

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
- `squeezy-llm`: provider abstraction plus native OpenAI/Anthropic/Google,
  Azure OpenAI, Bedrock, Ollama, OpenAI-compatible chat providers, and local
  OAuth credential sources.
- `squeezy-tools`: first-party tool specs and runtimes, checkpoints, shell
  sandbox integration, web tools, graph/search/read/edit tools, and MCP tool
  wrapping.
- `squeezy-skills`: local `SKILL.md` discovery/loading and built-in Squeezy
  help.
- `squeezy-hooks`: opt-in hook event types, payloads, registry, and dispatch
  helpers used by agent and skill integrations.
- `squeezy-mcp`: MCP server/client plumbing and protocol adapters.
- `squeezy-eval`: scenario-driven live-agent evaluation, replay, trace, and
  finding-report tooling.
- `squeezy-workspace`, `squeezy-parse`, `squeezy-graph`, `squeezy-rank`,
  `squeezy-store`, `squeezy-vcs`, `squeezy-telemetry`, `squeezy-harness`, and
  `squeezy-win-sandbox`: workspace discovery, parsers, graph state and optional
  watcher support, ranking helpers, split redb-backed local state,
  VCS/checkpoint support, anonymous telemetry, validation tasks, and the Windows
  OS-level shell sandbox (restricted-token and elevated tiers).

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

User-facing behavior belongs in `crates/squeezy-skills/external-docs/` because
those files are packaged with the `squeezy-skills` crate and embedded into
in-product help. Contributor workflow, implementation decisions, benchmark
oracles, release/deployment notes, and maintenance conventions belong in
`docs/internal/`.

When moving an external doc, update help topic citations in
`crates/squeezy-skills/src/help.rs`. The build-generated bundled-doc list is
checked by tests: every external doc must be bundled, every help citation must
point at a bundled path, and internal docs must not be bundled into normal
help.

## Release Process

The release workflow (`.github/workflows/release.yml`) is triggered by a
`v*` tag push or a manual `workflow_dispatch` with an existing tag.

The version contract is:

1. The git tag must match `vMAJOR.MINOR.PATCH[-PRERELEASE]`.
2. The tag (with the leading `v` stripped) must equal
   `workspace.package.version` in the root `Cargo.toml`.
3. Both are enforced by the workflow's `Validate release tag` and
   `Verify tag matches workspace version` steps, and again by
   `scripts/update_homebrew_formula.sh` as defense in depth.

When cutting a release, bump `workspace.package.version` in `Cargo.toml`
in the same commit that the release tag points at. Pushing a tag whose
version disagrees with `Cargo.toml` will fail the workflow before any
artifacts are built or published.

### Release Binary Hygiene

The root `Cargo.toml` keeps `[profile.release]` practical for source installs
(`lto = false`, `codegen-units = 16`). Distribution artifacts use
`[profile.dist]`, and `.github/workflows/release.yml` builds them with
`cargo build --profile dist -p squeezy --target ...`. The dist profile sets
`lto = true`, `codegen-units = 1`, and `strip = "symbols"`, so shipped archives
carry no Rust symbol table and no DWARF debug info. The reasons:

- Symbol names leak module paths (`squeezy_agent::Agent::run_turn`),
  build-host absolute source paths, and private type structure that
  Squeezy does not commit to as a public API.
- A stripped binary is a smaller release artifact and shrinks the
  surface for symbol-name-based vulnerability fingerprinting.
- User crash reports should rely on `squeezy doctor`, `--version`, and
  redacted session ids rather than on raw backtraces. Any future
  symbolicated telemetry must emit debug sidecars (`*.dSYM`, `*.dwp`,
  `*.pdb`) that are uploaded to a private symbol store, never bundled
  into the public release tarball or the Homebrew bottle.

Do not weaken dist artifacts to `strip = "debuginfo"` or remove the dist
`strip` directive without updating this section. If backtraces in user reports
become a hard requirement, prefer adding private debug sidecars over
re-enabling symbols in the public binary.

## Provider SDK Policy

An earlier rule of thumb said Squeezy should not depend on any vendor SDK
and should call provider APIs directly with `reqwest`. That guidance is
retired:

- **Vendor SDKs are allowed when they materially reduce auth, retry, or
  pagination complexity.** Bedrock is the existing example — SigV4 is
  not practical to reimplement, so `aws-sdk-bedrockruntime` and
  `aws-config` are the right tools.
- **Raw `reqwest` remains the default for simple bearer-token REST
  APIs** (Anthropic, OpenAI, Google, Azure OpenAI, Ollama, and most
  OpenAI-compatible routes). This is
  momentum, not principle. Do not rewrite an existing provider purely
  to add an SDK; only reach for one when the new provider's auth or
  protocol justifies it.
- **No embedded HTTP server, ever.** Squeezy is CLI/TUI only. Do not add
  `axum`, `warp`, `actix-web`, or any other framework that accepts inbound
  connections. The current `squeezy ask` bridge is the only acceptable
  Squeezy-owned local IPC: it uses a session-scoped Unix domain socket on Unix
  and a named pipe on Windows, never a network port.

## Non-Goals

Squeezy is a single-process Rust CLI/TUI. The following are explicit
non-goals; reviewers should reject PRs that move toward them.

- No app-server, daemon, or long-running background service. There is no
  process that outlives the user's interactive session.
- No SDK client crate, remote-control protocol, or proprietary
  cross-process API. The runtime surface is the `squeezy` binary and its
  subcommands; there is no parallel API surface for embedders.
- No new workspace members matching `*-server`, `*-client`,
  `*-protocol`, `*-daemon`, or `sdk-*`. The 19-crate layout under
  `crates/` is the intended scope.
- No embedded HTTP server or inbound network listener (see the
  "Provider SDK Policy" section above for the framework-level restatement of
  this rule). The only acceptable Squeezy-owned local IPC is the
  session-scoped `squeezy ask` socket/pipe bridge.

Callers that need to script Squeezy invoke the CLI directly
(`squeezy --prompt …` for one-shot non-interactive turns, or subcommands
like `squeezy sessions`, `squeezy repo`, `squeezy config`). External
tools that need to extend Squeezy's capabilities run as MCP servers
configured via `squeezy mcp add`; cross-process orchestration belongs to
MCP, not to a Squeezy-specific protocol.

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
   `crates/squeezy-skills/external-docs/LANGUAGES.md` with user-facing coverage
   and limitations.
