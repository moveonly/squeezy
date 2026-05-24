# Squeezy

A coding agent that treats cost, speed, and code understanding as first-class citizens.

Squeezy parses repositories and builds a persistent local semantic graph. The agent queries this graph through structured tools that return compact evidence packets — spans, hashes, confidence, freshness — instead of raw file dumps.

> **Status:** early development. The foundation TUI scaffold is runnable, provider/model selection is registry-backed, OpenAI, Anthropic, Gemini, Azure OpenAI, and Ollama adapters are available, and deterministic validation harness tasks run in CI. Graph-backed navigation is still planned. Committed decisions live in [`docs/`](docs).

## Cost

Every model token is a budgeted resource.

- **Context receipts** let re-reads return stubs that reference an earlier result instead of resending bytes.
- An **exploration compiler** translates model intent into a deterministic local query plan; only the final compact evidence packet ships to the model.
- A **cost broker** enforces per-turn caps on `grep`, raw reads, and tool calls, and routes trivial work to cheaper models.
- **Failure memory** keeps the agent from repeating dead-end searches across compactions.
- The static system prompt is held stable so provider caches actually hit.
- Current fallback tools use ignore-aware `grep`, path-only `glob`, compact
  search modes, spill handles, aggregate result budgets, and permission-gated
  `websearch`/`webfetch` for current external evidence.
- The tool-call saving roadmap is documented in [`docs/tool-call-saving-strategy.md`](docs/tool-call-saving-strategy.md).
- Anonymous product telemetry is documented in [`docs/TELEMETRY.md`](docs/TELEMETRY.md).
- Consented feedback and bug-report intake are documented in [`docs/FEEDBACK.md`](docs/FEEDBACK.md).
- Configuration is documented in [`docs/CONFIGURATION.md`](docs/CONFIGURATION.md), with provider details in [`docs/PROVIDERS.md`](docs/PROVIDERS.md).

## Speed

Latency is tracked along four axes:

- **Time-to-first-token**, by sending focused context rather than raw file dumps.
- **Task wall-clock**, by reducing tool calls and redo cycles.
- **Cold start**, by lazy indexing on first run and persisting the graph between sessions.
- **Tool-call latency**, by serving graph queries from local indexes, not network or compiler services.

## Code understanding

The semantic graph is the primary navigation surface; bounded grep is a labeled fallback.

- Every relationship carries a **confidence label** (`exact_syntax`, `import_resolved`, `candidate_set`, `external`, `unknown`).
- Every claim carries **provenance**: spans, hashes, parser/query origin, freshness.
- **Framework adapters** (planned) expose routes and system functions as graph nodes when a framework is detected.
- The **current branch diff** is first-class context: "what did I just change and what does it affect" is one query, not a search.
- Unsupported languages return structured `unsupported` / `partial` results rather than fabricated graph confidence. The current language coverage matrix lives in [`docs/LANGUAGES.md`](docs/LANGUAGES.md).

## Scope

Squeezy targets local semantic navigation across Rust, Python, Java, C#/.NET, Go, C/C++, and JavaScript/TypeScript. Initial platforms are macOS and Linux. The Linux release artifact is built for `x86_64-unknown-linux-musl` so it does not depend on glibc. The UI is a TUI. Squeezy is an MCP client: external MCP servers can be installed and consumed as tools.

Squeezy explicitly does not provide:

- a hosted service — it runs locally,
- an IDE plugin — the TUI is the only interface,
- LSP-backed navigation — the graph is lightweight, local, and agent optimized,
- a single-provider integration — bring your own key,
- an MCP server or remote API for its semantic graph — the graph is internal.

## License

See [LICENSE](LICENSE).

## Contributing

Build, test, clippy, and coverage commands are documented in [CONTRIBUTING.md](CONTRIBUTING.md).
