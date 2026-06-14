# Technical Debt Refactoring Audit

Date: 2026-06-14

This is a read-only audit of refactoring, simplification, and readability
opportunities in the current worktree. The audit split the codebase by major
ownership areas and used subagents for parallel subsystem review. The TUI
subagent failed during remote compaction, so TUI findings below are ba  sed on
the parent local scan rather than a completed subagent slice.

## Largest Hotspots

The largest files are the strongest short-term signals for refactoring
pressure:

- `crates/squeezy-tui/src/lib.rs`: 52,058 lines
- `crates/squeezy-tui/src/lib_tests.rs`: 52,018 lines
- `crates/squeezy-agent/src/lib.rs`: 20,200 lines
- `crates/squeezy-core/src/lib.rs`: 15,974 lines
- `crates/squeezy-tools/src/lib_tests.rs`: 14,322 lines
- `crates/squeezy-agent/src/lib_tests.rs`: 13,697 lines
- `crates/squeezy-graph/src/lib_tests.rs`: 9,743 lines
- `crates/squeezy-tools/src/lib.rs`: 7,494 lines
- `crates/squeezy-core/src/lib_tests.rs`: 7,189 lines
- `crates/squeezy-tools/src/graph_tools.rs`: 6,951 lines
- `crates/squeezy-cli/src/main.rs`: 5,519 lines
- `crates/squeezy-graph/src/lib.rs`: 5,363 lines
- `crates/squeezy-core/src/config_schema.rs`: 4,938 lines
- `crates/squeezy-vcs/src/lib.rs`: 4,763 lines

## Highest Priority Actions

1. Split the TUI monolith into owned surface, state, input, and render modules.
   `crates/squeezy-tui/src/lib.rs` already declares many feature modules at
   lines 81-198, but still keeps key dispatch, mouse routing, render dispatch,
   and `TuiApp` state in the same 52k-line file. Start by extracting the modal
   surface registry used by `handle_key` around line 8528 and `render_surfaces`
   around line 27238. Then move `TuiApp` state groups from line 47594 into
   domain-owned state structs.

2. Split the agent turn runtime into phase modules. `TurnRuntime::run` begins
   around `crates/squeezy-agent/src/lib.rs:7651` and spans request assembly,
   compaction, routing, streaming, tool execution, persistence, and terminal
   state. Extract `turn/request.rs`, `turn/stream.rs`, `turn/tool_round.rs`,
   and `turn/terminal.rs`. This should happen before deeper behavioral changes
   because it lowers the risk of future routing, tool, and persistence work.

3. Centralize config metadata and path resolution. `squeezy-core/src/lib.rs`
   owns config templates around line 11128, config source/path loading around
   11783-11922, and broad public config types in the same file. `squeezy-cli`
   mirrors parts of this logic in `main.rs` around 1239 and config explain logic
   around 1342-1583. Introduce a core `ConfigLocator` / `ConfigTiers` and make
   templates and `config_schema::FieldMeta` share one source of truth.

4. Introduce first-party tool descriptors. Tool specs, permission scopes,
   permission request construction, and execution dispatch are maintained in
   separate blocks in `crates/squeezy-tools/src/lib.rs` around 2047, 2338,
   2370, and 3441. Add a `ToolDescriptor` catalog that owns spec, permission
   builder, prepare hook, and executor registration.

5. Make language and provider metadata single-source. Language support is
   currently spread across core language family tables, parse backends, graph
   backends, and workspace selectors. Provider/auth metadata is similarly split
   across CLI auth, doctor, core schema, and LLM provider modules. Add registry
   tests that force every supported language/provider to declare its aliases,
   config metadata, runtime behavior, and help/docs exposure in one place.

## Category Findings

### 1. TUI, Input, Rendering, And State

Recommended actions:

- Extract a modal surface registry. `handle_key` around
  `crates/squeezy-tui/src/lib.rs:8528` and `render_surfaces` around
  `crates/squeezy-tui/src/lib.rs:27238` both contain long ordered chains for
  modal ownership. Encode each surface as a descriptor with `is_open`,
  `handle_key`, `render`, and optional mouse handling. This reduces ordering
  drift between keyboard and render paths.
- Split render functions by surface. Rendering starts at
  `crates/squeezy-tui/src/lib.rs:27188`, then dozens of `render_*_surface`
  helpers remain in `lib.rs`. Move repeated surface rendering into existing
  feature modules such as `clipboard_history`, `keybinding_editor`,
  `scratchpad`, `theme_editor`, `transcript_surface`, and `subagent_compare`.
- Break `TuiApp` into nested state groups. The state struct starts at
  `crates/squeezy-tui/src/lib.rs:47594` and contains config mirrors,
  transcript geometry caches, selection/search/jump state, overlay state,
  feature panel state, queue state, notifications, keymaps, and click
  registries. Introduce domain structs such as `ComposerState`,
  `TranscriptViewState`, `OverlayState`, `NavigationState`, and
  `PromptQueueState`.
- Move test helpers out of the bottom of `lib_tests.rs`. Shared fixtures start
  around `crates/squeezy-tui/src/lib_tests.rs:21315`, with `test_app`,
  `test_agent`, `render_to_string`, clipboard doubles, temp workspace helpers,
  and common transcript helpers. Promote these into `src/testing.rs` or a
  crate-local `test_support` module.
- Split `lib_tests.rs` by feature owner. The 52k-line test file should be
  migrated incrementally into paired `*_tests.rs` files beside the extracted
  modules. Start with surfaces that already have clear owners: smart split,
  clipboard history, keybinding editor, prompt queue, transcript overlay, and
  subagent panes.

Risk: medium to high, mostly due to cross-surface ordering and state mutation.
Start with mechanical moves and parity tests before changing behavior.

### 2. Agent, Turns, Sessions, And Store

Recommended actions:

- Split `TurnRuntime::run` into phases. Anchors:
  `crates/squeezy-agent/src/lib.rs:7651`, `:7946`, `:8943`, `:10073`,
  `:10366`.
- Centralize session persistence through a `ConversationCommit` or
  `SessionPersistenceSnapshot`. Normal turns, help turns, local shell turns,
  soft completions, failed turns, and cancelled turns persist overlapping
  metadata through separate paths around `crates/squeezy-agent/src/lib.rs:6195`,
  `:6410`, `:10653`, and `:11054`.
- Extract bootstrap services from `Agent`. `Agent` starts at
  `crates/squeezy-agent/src/lib.rs:1415`; construction/resume setup spans
  around `:1655`, `:1719`, and `:1939`. Separate session bootstrap, service
  wiring, context ingestion, telemetry, tool registry, and calibration.
- Make compaction eligibility a typed decision. `context_compaction.rs:145`
  owns the compaction predicate, while `squeezy-agent/src/lib.rs:7946` mirrors
  the hook gate. Add a `CompactionDecision` type and make hooks consume it.
- Split `crates/squeezy-store/src/sessions.rs` into store, handle, writer,
  replay, index, and cleanup modules. Current anchors include store/index work
  around `:706`, `:938`, `:1151`, handle/writer around `:1389`, `:1487`,
  `:1961`, and replay around `:2168`.
- Finish migration from string event kinds to typed session events. Typed
  storage exists around `crates/squeezy-store/src/sessions.rs:2018` and
  `:2780`, but agent logging still accepts string event kinds around
  `crates/squeezy-agent/src/lib.rs:19370`.

Risk: medium. The safest order is compaction decision, typed logging helpers,
then turn-runtime phase extraction.

### 3. Core, CLI, Config, Doctor, And Auth

Recommended actions:

- Split `crates/squeezy-core/src/lib.rs` into domain modules: config paths,
  config templates, config sources, provider settings, transcript,
  attachments, metrics, semantic IDs, and permission policy. Anchors:
  `crates/squeezy-core/src/lib.rs:16`, `:11128`, `:12069`.
- Make config templates and schema share metadata. Templates in
  `crates/squeezy-core/src/lib.rs:11128`, `:11411`, `:11593` duplicate
  information from `crates/squeezy-core/src/config_schema.rs:405`.
- Move config explain parsing/source lookup out of CLI. CLI logic around
  `crates/squeezy-cli/src/main.rs:1342`, `:1460`, and `:1583` should live
  behind `squeezy_core::config_schema`.
- Break up `crates/squeezy-cli/src/main.rs`. The file owns clap definitions,
  runtime boot, TUI startup, config, MCP, sessions, feedback, parse, and
  formatting. Keep top-level dispatch thin and move command families into
  modules.
- Convert doctor checks into a registry. `crates/squeezy-cli/src/doctor.rs:266`,
  `:345`, and `:474` manually sequence checks and selector gating. Introduce
  `DoctorCheck { name, needs_config, run }`.
- Share provider/auth metadata across auth, doctor, schema, and provider
  registry. Anchors: `crates/squeezy-cli/src/auth.rs:26`, `:873`,
  `crates/squeezy-cli/src/doctor.rs:692`,
  `crates/squeezy-core/src/config_schema.rs:356`.
- Split `auth.rs` into inline-key, OpenAI Codex OAuth, GitHub Copilot OAuth,
  Anthropic OAuth, and rendering modules. Anchors:
  `crates/squeezy-cli/src/auth.rs:383`, `:585`, `:905`, `:1037`.

Risk: medium. Favor metadata extraction and registry parity tests before
moving large command handlers.

### 4. Tools, Shell, Sandbox, Graph Tools, And MCP

Recommended actions:

- Add a first-party `ToolDescriptor` catalog. Use it to join tool schema,
  permission metadata, permission request construction, and executor dispatch.
  This directly addresses drift across `crates/squeezy-tools/src/lib.rs:2047`,
  `:2338`, `:2370`, and `:3441`.
- Split `crates/squeezy-tools/src/lib.rs` by responsibility: protocol types,
  registry runtime, MCP bridge, verify planning, file mutation/editing, path
  resolution, output storage, and permission catalog. Anchors include `:1168`,
  `:3530`, `:4850`, and `:5598`.
- Make shell parsing produce one structured policy input. `shell_parse.rs`
  already has `CommandUnit`-style structure, but safety and permission policy
  still duplicate token scanning. Drive Plan-mode, permission, pre-classifier,
  and write-target checks from the parsed command units.
- Decompose `shell.rs` into `ShellPolicyGate`, `ShellRunner`,
  `ShellFallbackController`, `ShellAskServer`, and `ShellOutputCapture`.
  Current anchors include `crates/squeezy-tools/src/shell.rs:225`, `:1052`,
  `:1960`, `:2126`, and `:2320`.
- Split sandbox planning by platform. `crates/squeezy-tools/src/shell_sandbox.rs`
  mixes macOS, Linux, Windows, probes, planning, and runtime metadata. Introduce
  typed `SandboxBackend` / `SandboxPosture` returned by `sandbox/macos.rs`,
  `sandbox/linux.rs`, and `sandbox/windows.rs`.
- Split `crates/squeezy-mcp/src/lib.rs` into registry, transport, tool palette,
  schema compaction, elicitation, and resources. Anchors include `:301`,
  `:2122`, `:2523`, `:2894`, and `:3007`.
- Extract packet, read-slice, diff-range, filter, and executor helpers from
  `crates/squeezy-tools/src/graph_tools.rs`. Anchors include `:33`, `:450`,
  `:2386`, `:3731`, and `:5251`.

Risk: medium to high for shell execution and sandboxing; low to medium for
module moves and descriptor parity tests.

### 5. LLM Providers, Retry, OAuth, Eval, And Harness

Recommended actions:

- Split provider option lowering out of provider bodies. `LlmRequest` around
  `crates/squeezy-llm/src/lib.rs:136` carries common and provider-specific
  knobs, while OpenAI, Google, Ollama, Anthropic, Bedrock, and compatible
  providers each lower options manually. Add common option structs plus
  provider-specific lowerers.
- Extract a shared SSE stream driver for Responses-style providers. OpenAI
  around `crates/squeezy-llm/src/openai.rs:657`, compatible around
  `crates/squeezy-llm/src/compatible.rs:728`, and OpenAI Codex around
  `crates/squeezy-llm/src/oauth/openai_codex.rs:1285` repeat auth retry,
  status handling, SSE decode, idle timeout, cancel, and completion checks.
- Split `retry.rs` into policy, request retry, stream retry, and classifiers.
  Anchors: `crates/squeezy-llm/src/retry.rs:21`, `:126`, `:414`, `:740`.
- Normalize OpenAI-compatible preset quirks into a `CompatPolicy` row. Current
  special cases appear around `crates/squeezy-llm/src/compatible.rs:67`,
  `:608`, `:1576`, and `:1602`.
- Consolidate OAuth/token plumbing. PKCE, token POST, storage, and encoding are
  duplicated across `oauth/pkce.rs`, OpenAI Codex, Anthropic, and Vertex. Also
  update stale Vertex comments around `crates/squeezy-llm/src/oauth/vertex.rs:22`.
- Add a crate-local `LlmRequest` test builder. Tests repeatedly spell every
  field in `openai_tests.rs`, `anthropic_tests.rs`, `google_tests.rs`, and
  `lib_tests.rs`.
- Create shared eval/harness config sanitizers and agent-event projection.
  Anchors: `crates/squeezy-eval/src/driver.rs:163`, `:578`, `:2091`, `:2595`,
  `crates/squeezy-harness/src/lib.rs:438`, `:595`, `:729`.

Risk: medium. Start with test builders and retry module splits, then move into
shared streaming and provider option lowering.

### 6. Semantic Graph, Parsing, Workspace, Rank, And VCS

Recommended actions:

- Split `SemanticGraph` responsibilities. It currently owns graph data, query
  APIs, indexes, resolver slots, project facts, cargo facts, cache hydration,
  and refresh reporting. Anchors: `crates/squeezy-graph/src/lib.rs:405`,
  `:2107`, `:2860`.
- Make language registration single-source. Language metadata is split across
  `crates/squeezy-core/src/lib.rs:15412`,
  `crates/squeezy-parse/src/backend.rs:96`,
  `crates/squeezy-graph/src/backend.rs:71`, and
  `crates/squeezy-workspace/src/lib.rs:940`.
- Stop using `crates/squeezy-parse/src/languages/rust.rs` as a shared helper
  bag. Other language modules import Rust helpers, while generic, JS/TS, Java,
  Python, and Rust-specific helpers are intermixed. Move generic helpers to
  `languages/common.rs`.
- Abstract repeated parser visitor mechanics for missing-node handling,
  parent/owner propagation, symbol push, call/reference/body-hit traversal, and
  parsed-file assembly.
- Clarify the phased resolver boundary. `cross_file.rs` says phased structures
  are populated and cached, but active resolution remains a long chain in
  `resolution.rs`. Either migrate one language family fully or narrow
  `cross_file` to cache/foundation scope.
- Extract refresh planning/report helpers from `SemanticGraph::refresh_now`.
  Anchors: `crates/squeezy-graph/src/lib.rs:3244`, `:3277`, `:3318`.
- Unify ranking tokenization primitives across fuzzy, path, and BM25 ranking.
  Anchors: `crates/squeezy-rank/src/fuzzy.rs:111`,
  `path_rank.rs:145`, `bm25_rank.rs:130`.
- Split VCS diff/checkpoint/rollback/git-command plumbing out of
  `crates/squeezy-vcs/src/lib.rs`. Anchors: `:74`, `:1108`, `:1405`,
  `crates/squeezy-vcs/src/worktree.rs:181`.

Risk: medium to high for resolver migration; low to medium for language
metadata tests and tokenizer extraction.

### 7. Skills, Help, Bundled Docs, And Test Layout

Recommended actions:

- Move inline `trigger_tests` out of `crates/squeezy-skills/src/lib.rs:2832`.
  `python3 scripts/check_test_layout.py` currently passes, so the checker has a
  blind spot: it should reject any inline `#[cfg(test)] mod ... {` block, not
  only `mod tests`.
- Split `crates/squeezy-skills/src/lib.rs` into catalog, frontmatter,
  manifest, hooks, installer, and validation modules. Anchors: `:515`, `:1422`,
  `:2261`, `:2327`.
- Split `crates/squeezy-skills/src/lib_tests.rs` by module owner. Anchors:
  `:167`, `:735`, `:2357`, `:2911`.
- Make bundled docs generation directory-driven. `build.rs` hardcodes markdown
  files, while help tests scan `external-docs/` to catch omissions. Generate
  `BUNDLED_DOCS` from sorted `external-docs/*.md` with explicit excludes only
  when needed.
- Unify slash-command help with the live TUI registry. `SLASH_COMMAND_HELP_TABLE`
  in `crates/squeezy-skills/src/help.rs:1006` duplicates command metadata from
  the TUI registry in `crates/squeezy-tui/src/input.rs:124`.
- Derive volatile `/help` topic lists from registries or generated docs inputs
  instead of hand-listing providers, languages, and commands.
- Clarify whether `crates/squeezy-skills/tests/artifacts/skills/rust-code-navigation/SKILL.md`
  is a fixture or shipped example. Either move it to docs/examples or add an
  integration test that loads it as the canonical artifact.
- Finish consolidating costly provider integration helpers in
  `crates/squeezy-llm/tests/common/mod.rs` and document `tests/common` as the
  approved integration-test helper exception.

Risk: mostly low to medium. These are good early wins because they improve the
guardrails that later refactors rely on.

## Suggested Implementation Sequence

1. Guardrails first:
   - Tighten `scripts/check_test_layout.py` for inline test modules.
   - Add registry parity tests for languages, provider/auth metadata, tool
     descriptors, and slash-command help.
   - Add shared test builders for `LlmRequest`, agent providers, TUI app/agent
     fixtures, and session stores.

2. Low-risk mechanical splits:
   - Move skills tests and modules.
   - Move tools sandbox/shell tests into owner modules.
   - Split VCS and store modules without behavior changes.
   - Make bundled docs generation directory-driven.

3. Descriptor and metadata unification:
   - ToolDescriptor catalog.
   - ConfigLocator / ConfigTiers.
   - ProviderAuthMeta / CompatPolicy.
   - Language registry metadata.

4. High-risk runtime decompositions:
   - TUI modal surface registry and state grouping.
   - Agent turn runtime phase modules.
   - Shell runner/fallback/sandbox platform split.
   - LLM shared SSE driver and option lowering.
   - Semantic graph resolver boundary cleanup.

## Verification Strategy

For each refactor PR, prefer narrow behavior-preserving moves with focused
tests:

- Module split PRs: run the owning crate's existing tests and add only
  exhaustiveness/fixture tests when new structure needs a guard.
- Descriptor/registry PRs: add table-driven tests that fail when metadata
  drifts.
- Runtime decomposition PRs: preserve existing behavior tests, then add one or
  two seam tests for the newly extracted decision type or phase.
- TUI PRs: keep screenshot/string render tests around affected surfaces and add
  key/render parity tests when modal ordering changes.

## Coverage Notes

- Completed subagent slices: agent/store, core/CLI/config/auth/doctor,
  tools/shell/sandbox/MCP, graph/parse/workspace/rank/VCS, LLM/eval/harness,
  skills/help/test layout.
- TUI subagent failed during remote compaction; TUI recommendations are based
  on local parent inspection and file-size/function-structure scans.
- This audit did not modify Rust code and did not run the full test suite.
