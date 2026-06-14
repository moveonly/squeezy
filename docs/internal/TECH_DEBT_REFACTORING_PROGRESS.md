# Technical Debt Refactoring Progress

Source audit: `docs/internal/TECH_DEBT_REFACTORING_AUDIT.md` (2026-06-14)

Status reflects coverage in `origin/main..HEAD` on branch
`tech-debt-audit-refactor` as of `1e5434ca`.

Legend: `Done` means the current branch directly completes the item. `Partial`
means the branch adds a guardrail, helper, or first slice but leaves the audit
action incomplete. `Open` means no current-branch coverage found.

## Highest Priority Actions

| Status | Owner slice | Action | Evidence | PR/commit |
| --- | --- | --- | --- | --- |
| Partial | TUI | Split the TUI monolith into owned surface, state, input, and render modules. | Added descriptor-backed inline prompt routing, moved plan-choice/feedback rendering to owner modules, grouped plan runtime fields under `PlanUiState`, and moved selected tests/helpers; the full TUI monolith split remains open. | `techdebt-tui-wave2` |
| Open | Agent/store | Split the agent turn runtime into request, stream, tool-round, and terminal phases. | Current branch centralizes compaction decisions only; `TurnRuntime::run` remains unsplit. | TBD |
| Partial | Core/CLI/config | Centralize config metadata and path resolution. | Added core `ConfigInitScope`, `ConfigInitTarget`, and `config_init_target`; schema/template unification remains open. | `df298934`, `9c777a5b` |
| Done | Tools/shell/MCP | Introduce first-party tool descriptors. | Added `crates/squeezy-tools/src/catalog.rs` with first-party descriptors, catalog-built specs, prepare hooks, permission profiles/scopes, and executor routing; `permission_scope`, `permission_request`, and dispatch now derive from descriptor rows. | `b0fdfc27`, `4788440c`, TBD |
| Partial | Graph/language/provider | Make language and provider metadata single-source. | Added language registry coverage across core, parse, graph, and workspace; provider/auth metadata remains split. | `d8170431` |

## Category Recommended Actions

| Status | Owner slice | Action | Evidence | PR/commit |
| --- | --- | --- | --- | --- |
| Partial | TUI | Extract a modal surface registry. | `modal::SurfaceDescriptor` now backs keymap, paste, prompt-queue drain, and inline decision render routing, including pending plan choice and feedback; a full descriptor-owned `is_open` / `handle_key` / `render` registry remains open. | `techdebt-tui-wave2` |
| Partial | TUI | Split render functions by surface. | Moved plan-choice line rendering to `plan_choice.rs` and feedback prompt line rendering to `feedback_prompt.rs`; most surface render helpers still remain in `lib.rs`. | `techdebt-tui-wave2` |
| Partial | TUI | Break `TuiApp` into nested state groups. | Added `plan_choice::PlanUiState` and migrated active plan id, pending choice, Build handoff, pause, and resume marker fields under `TuiApp::plan`; other TUI state clusters remain on `TuiApp`. | `techdebt-tui-wave2` |
| Done | TUI | Move shared test helpers out of the bottom of `lib_tests.rs`. | Added `crates/squeezy-tui/src/test_support.rs` for shared app/config/agent/temp workspace/render/clipboard helpers and removed the duplicate helper block from `lib_tests.rs`. | `techdebt-tui-wave2` |
| Partial | TUI | Split `lib_tests.rs` by feature owner. | Moved actionable command-hyperlink tests into the existing `crates/squeezy-tui/src/help_links_tests.rs` owner; most `lib_tests.rs` coverage still remains unsplit. | `techdebt-tui-wave2` |
| Open | Agent/store | Split `TurnRuntime::run` into phases. | No turn phase modules added. | TBD |
| Open | Agent/store | Centralize session persistence through a commit/snapshot type. | No `ConversationCommit` or `SessionPersistenceSnapshot` introduced. | TBD |
| Open | Agent/store | Extract bootstrap services from `Agent`. | Agent construction/service wiring remains in `crates/squeezy-agent/src/lib.rs`. | TBD |
| Done | Agent/store | Make compaction eligibility a typed decision. | Added `ContextCompactionDecision` and routed auto-compaction through `context_compaction_decision`. | `6d185457`, `50a76f24` |
| Partial | Agent/store | Split `crates/squeezy-store/src/sessions.rs` into store, handle, writer, replay, index, and cleanup modules. | Extracted replay state and JSONL helpers into `sessions_replay.rs`; other store areas remain in `sessions.rs`. | `eb75270e` |
| Open | Agent/store | Finish migration from string event kinds to typed session events. | No agent logging migration away from string event kinds in current branch. | TBD |
| Open | Core/CLI/config | Split `crates/squeezy-core/src/lib.rs` into domain modules. | Config init target helper was added, but no core module split. | TBD |
| Open | Core/CLI/config | Make config templates and schema share metadata. | Template/schema metadata still not unified. | TBD |
| Open | Core/CLI/config | Move config explain parsing/source lookup out of CLI. | CLI explain logic remains in `crates/squeezy-cli/src/main.rs`. | TBD |
| Open | Core/CLI/config | Break up `crates/squeezy-cli/src/main.rs`. | No command-family modules extracted. | TBD |
| Open | Core/CLI/config | Convert doctor checks into a registry. | No `DoctorCheck` registry added. | TBD |
| Open | Core/CLI/config | Share provider/auth metadata across auth, doctor, schema, and provider registry. | No provider/auth metadata registry added. | TBD |
| Open | Core/CLI/config | Split `auth.rs` into provider/auth-flow and rendering modules. | Only auth test imports changed; auth flows remain in `auth.rs`. | TBD |
| Done | Tools/shell/MCP | Add a first-party `ToolDescriptor` catalog. | `catalog.rs` owns descriptor rows for specs, prepare hooks, permission profiles/scopes, and executor selection; runtime approval and dispatch consult the descriptor rather than parallel string tables. | `b0fdfc27`, `4788440c`, TBD |
| Partial | Tools/shell/MCP | Split `crates/squeezy-tools/src/lib.rs` by responsibility. | Moved first-party catalog/spec construction/descriptor dispatch out to `catalog.rs`; blocker: file mutation, checkpoint, read/write, web, output storage, and registry state still share `lib.rs`, and splitting them safely needs a broader public-helper boundary pass. | TBD |
| Done | Tools/shell/MCP | Make shell parsing produce one structured policy input. | Added `ShellPolicyInput` derived from tree-sitter `CommandUnit` records; `analyze_shell_command` now classifies segments rendered from that structured input when parser-backed units exist. | TBD |
| Partial | Tools/shell/MCP | Decompose `shell.rs` into policy, runner, fallback, ask-server, and output-capture components. | Extracted nested ask IPC into `shell_ask_server.rs` and bounded pipe/raw-sidecar capture into `shell_capture.rs`; blocker: runner, fallback, and policy denial still share spawn/timeout/sandbox-health state in `shell.rs`, so finishing this row needs a larger state-boundary split. | TBD |
| Partial | Tools/shell/MCP | Split sandbox planning by platform. | Added `shell_sandbox/backend.rs` with linux/macos/windows/unsupported backend metadata modules consumed by approval metadata; blocker: `prepare_shell_sandbox_plan` still owns cross-platform command argument construction, probing, and fallback state in `shell_sandbox.rs`. | TBD |
| Partial | Tools/shell/MCP | Split `crates/squeezy-mcp/src/lib.rs` into registry, transport, palette, schema compaction, elicitation, and resources. | Added `transport.rs`, `elicitation.rs`, and `resources.rs` alongside existing `schema_compaction.rs`; blocker: `McpClientRegistry` orchestration and palette normalization/cache persistence still live in `lib.rs`. | `2cc94eb8`, TBD |
| Done | Tools/shell/MCP | Extract packet, read-slice, diff-range, filter, and executor helpers from `graph_tools.rs`. | Added `graph_tools_packets.rs`, `graph_tools_read_slice.rs`, `graph_tools_diff_ranges.rs`, `graph_tools_filters.rs`, and `graph_tools_executor.rs`; graph entrypoint, path filters, read-slice args/context, and diff range math now live outside `graph_tools.rs`. | TBD |
| Open | LLM/eval/harness | Split provider option lowering out of provider bodies. | No provider option lowerers added. | TBD |
| Open | LLM/eval/harness | Extract a shared SSE stream driver for Responses-style providers. | No shared stream driver added. | TBD |
| Open | LLM/eval/harness | Split `retry.rs` into policy, request retry, stream retry, and classifiers. | No retry module split in current branch. | TBD |
| Open | LLM/eval/harness | Normalize OpenAI-compatible preset quirks into a `CompatPolicy` row. | No `CompatPolicy` added. | TBD |
| Partial | LLM/eval/harness | Consolidate OAuth/token plumbing and update stale Vertex comments. | Vertex comments were updated, but OAuth/token plumbing is not consolidated. | `4f91e351` |
| Done | LLM/eval/harness | Add a crate-local `LlmRequest` test builder. | Added `crates/squeezy-llm/src/test_support.rs` with `LlmRequestBuilder` and migrated tests to it. | `4f91e351`, `a62acf8f` |
| Open | LLM/eval/harness | Create shared eval/harness config sanitizers and agent-event projection. | No eval/harness sanitizer or projection extraction in current branch. | TBD |
| Open | Graph/parse/workspace/rank/VCS | Split `SemanticGraph` responsibilities. | No graph storage/query/index/resolver split in current branch. | TBD |
| Partial | Graph/parse/workspace/rank/VCS | Make language registration single-source. | Added registry coverage across language families, parse backends, graph backends, and workspace classification; metadata remains spread across registries. | `d8170431` |
| Open | Graph/parse/workspace/rank/VCS | Stop using Rust language parsing helpers as a shared helper bag. | No `languages/common.rs` extraction in current branch. | TBD |
| Open | Graph/parse/workspace/rank/VCS | Abstract repeated parser visitor mechanics. | No parser visitor abstraction added. | TBD |
| Open | Graph/parse/workspace/rank/VCS | Clarify the phased resolver boundary. | No resolver boundary migration or scope narrowing in current branch. | TBD |
| Open | Graph/parse/workspace/rank/VCS | Extract refresh planning/report helpers from `SemanticGraph::refresh_now`. | No refresh helper extraction in current branch. | TBD |
| Done | Graph/parse/workspace/rank/VCS | Unify ranking tokenization primitives across fuzzy, path, and BM25 ranking. | Added `crates/squeezy-rank/src/tokens.rs` and routed fuzzy, path, and BM25 rankers through it. | `611cadce` |
| Partial | Graph/parse/workspace/rank/VCS | Split VCS diff/checkpoint/rollback/git-command plumbing out of `crates/squeezy-vcs/src/lib.rs`. | Extracted git command helpers into `git_command.rs`; diff/checkpoint/rollback plumbing remains in `lib.rs`. | `a8ed0eb6`, `b2dd984b` |
| Done | Skills/help/test layout | Move inline `trigger_tests` out of `crates/squeezy-skills/src/lib.rs` and close the checker blind spot. | Moved trigger tests to `lib_tests.rs` and updated `scripts/check_test_layout.py` to reject inline `#[cfg(test)] mod ...`. | `d3e2dad8` |
| Open | Skills/help/test layout | Split `crates/squeezy-skills/src/lib.rs` into catalog, frontmatter, manifest, hooks, installer, and validation modules. | No skills module split in current branch. | TBD |
| Open | Skills/help/test layout | Split `crates/squeezy-skills/src/lib_tests.rs` by module owner. | Trigger tests moved into `lib_tests.rs`, but the test file was not split by owner. | TBD |
| Done | Skills/help/test layout | Make bundled docs generation directory-driven. | Replaced hardcoded docs list in `crates/squeezy-skills/build.rs` with sorted `external-docs/*.md` discovery. | `d3e2dad8` |
| Open | Skills/help/test layout | Unify slash-command help with the live TUI registry. | No shared slash-command registry integration added. | TBD |
| Open | Skills/help/test layout | Derive volatile `/help` topic lists from registries or generated docs inputs. | No generated topic derivation added. | TBD |
| Open | Skills/help/test layout | Clarify whether the `rust-code-navigation` skill artifact is fixture or shipped example. | No fixture/example move or canonical artifact test added. | TBD |
| Open | Skills/help/test layout | Finish consolidating costly provider integration helpers in `crates/squeezy-llm/tests/common/mod.rs`. | No `tests/common` provider helper consolidation in current branch. | TBD |
