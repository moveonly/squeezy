# Squeezy thesis

Squeezy is a coding agent that treats **cost**, **speed**, and **code
understanding** as first-class citizens rather than emergent properties
of an unbounded loop.

## Premise

Most coding-agent failures look like cost failures. The agent re-reads
the same files, calls grep ten times across the same corpus, fans out
into dead-end edits, and pages back through the same context after each
compaction. Even a model that is fast and cheap per call becomes slow
and expensive when the loop wastes its calls.

Squeezy is the bet that the right place to fix this is the substrate
the agent sits on, not the model itself: give the agent a persistent
local semantic graph, structured tools that return small evidence
packets, and a cost broker that says no to wasteful work. The model
gets to do the part it is genuinely good at — reading evidence and
choosing the next step — and the substrate handles everything that is
deterministic.

## Cost

Every model token is a budgeted resource.

- **Context receipts** let re-reads return stubs that reference an
  earlier result instead of resending bytes.
- An **exploration compiler** translates model intent into a
  deterministic local query plan; only the final compact evidence
  packet ships to the model.
- A **cost broker** enforces per-turn caps on `grep`, raw reads, tool
  calls, round input size, and session spend.
- A **turn router** sends obvious low-risk work to cheaper models and
  escalates back to the parent model when a turn stops looking simple.
- **Failure memory** keeps the agent from repeating dead-end searches
  across compactions.
- The static system prompt is held stable so provider caches actually
  hit.
- Fallback tools use ignore-aware `grep`, path-only `glob`, compact
  search modes, spill handles, aggregate result budgets, and
  permission-gated `websearch` / `webfetch` for current external
  evidence.

## Speed

Latency is tracked along four axes:

- **Time-to-first-token**, by sending focused context rather than raw
  file dumps.
- **Task wall-clock**, by reducing tool calls and redo cycles.
- **Cold start**, by lazy indexing on first run and persisting the
  graph between sessions.
- **Tool-call latency**, by serving graph queries from local indexes,
  not network or compiler services.

## Code understanding

The semantic graph is the primary navigation surface; bounded grep is
a labeled fallback.

- Every relationship carries a **confidence label** such as
  `exact_syntax`, `import_resolved`, `heuristic`, `candidate_set`,
  `external`, `macro_opaque`, `conditional_unknown`, `unsupported`,
  `stale`, or `partial`.
- Graph symbols and edges carry **provenance** and freshness internally:
  spans, hashes, parser/query origin, and refresh state. Model-visible
  packets keep the compact evidence needed for decisions and drop
  decorative provenance fields when they would waste tokens.
- **Framework-aware extensions** can expose routes and system
  functions as graph nodes when a supported adapter exists.
- The **current branch diff** is first-class context: "what did I
  just change and what does it affect" is one query, not a search.
- Unsupported languages return structured `unsupported` / `partial`
  results rather than fabricated graph confidence. The current
  language coverage matrix lives in
  [`crates/squeezy-skills/external-docs/LANGUAGES.md`](../crates/squeezy-skills/external-docs/LANGUAGES.md).

## Scope

Squeezy targets local semantic navigation across Rust, Python, Java,
Kotlin, Scala, C#/.NET, Go, C/C++, JavaScript/TypeScript, PHP, Ruby,
Swift, and Dart. Supported platforms are macOS, Linux, and Windows.
Release archives are built for macOS Intel and Apple Silicon, Linux
x86_64 and ARM64 musl, and Windows x86_64 MSVC. The Linux artifacts do
not depend on glibc. The UI is a TUI. Squeezy is an MCP client:
external MCP servers can be installed and consumed as tools.

## Non-goals

Squeezy explicitly does not provide:

- a hosted service — it runs locally,
- an app-server or any embedded HTTP server — the binary is CLI/TUI
  only,
- an IDE plugin — the TUI is the only interface,
- LSP-backed navigation — the graph is lightweight, local, and
  agent-optimized,
- a single-provider integration — bring your own key,
- an MCP server or remote API for its semantic graph — the graph is
  internal.

For deeper reading, the agent approach is documented in
[`crates/squeezy-skills/external-docs/AGENT_APPROACH.md`](../crates/squeezy-skills/external-docs/AGENT_APPROACH.md),
the tool surface in
[`crates/squeezy-skills/external-docs/TOOLS.md`](../crates/squeezy-skills/external-docs/TOOLS.md),
and the
tool-call saving roadmap in
[`crates/squeezy-skills/external-docs/tool-call-saving-strategy.md`](../crates/squeezy-skills/external-docs/tool-call-saving-strategy.md).
Contributor-facing internals live under
[`docs/internal/`](internal).
