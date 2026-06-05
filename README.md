# Squeezy

A coding agent that treats cost, speed, and code understanding as first-class
citizens. Squeezy parses repositories into a persistent local semantic graph and
queries that graph through structured tools that return compact evidence
packets — spans, hashes, confidence, freshness — instead of raw file dumps.

> **Status:** early v0 development. The CLI/TUI is runnable; graph-backed
> navigation, deterministic validation harnesses, local help, sessions/resume,
> checkpoints, provider routing, MCP, skills, feedback/reporting, and telemetry
> are implemented and still evolving. Provider coverage changes faster than this
> README, so use
> [`PROVIDERS.md`](crates/squeezy-skills/external-docs/PROVIDERS.md) for exact
> provider ids, defaults, environment variables, and model metadata. Broadly:
>
> - **Aggregators (one key, many models):** OpenRouter, Vercel AI Gateway,
>   PortKey.
> - **First-party vendor APIs (single vendor):** OpenAI, Anthropic, Google
>   Gemini.
> - **Cloud-platform hosts:** Amazon Bedrock, Azure OpenAI, and Google Vertex AI.
> - **Subscription/auth-backed providers:** OpenAI Codex and GitHub Copilot.
> - **Local or self-hosted runtimes:** Ollama, LM Studio, vLLM, llama.cpp, and any
>   OpenAI-compatible endpoint.
> - **Other OpenAI-compatible services:** Groq, xAI, DeepSeek, Mistral,
>   Together AI, Fireworks AI, Cerebras, DeepInfra, Baseten, Cloudflare Workers
>   AI, Cloudflare AI Gateway, and similar presets.

The **why** lives in [`docs/THESIS.md`](docs/THESIS.md). User docs live in
[`crates/squeezy-skills/external-docs/`](crates/squeezy-skills/external-docs)
(co-located with the crate that bundles them into the binary at build time)
and contributor docs live in [`docs/internal/`](docs/internal).

## Install

One-line installer (macOS and Linux):

```sh
curl -fsSL https://raw.githubusercontent.com/esqueezy/squeezy/main/install.sh | sh
```

On macOS, Homebrew is also supported:

```sh
brew install esqueezy/tap/squeezy
```

Rust users can install with Cargo:

```sh
cargo install squeezy --locked
```

Tagged releases also publish macOS Intel, macOS Apple Silicon, Linux x86_64
musl, Linux aarch64 musl, and Windows x86_64 archives. Full install, first-run, upgrade, and
uninstall instructions are in [`INSTALL.md`](crates/squeezy-skills/external-docs/INSTALL.md).

## Quickstart

```sh
squeezy doctor                    # diagnose configuration and providers
squeezy config init --user        # write the default user settings file
squeezy config inspect            # print the effective merged configuration
squeezy --list-providers          # quick provider table
squeezy providers list            # provider registry and model counts
squeezy sessions list             # recent local sessions
squeezy --resume                  # open the resume picker

# Fastest path: one credit, every frontier model (recommended)
export OPENROUTER_API_KEY=...     # https://openrouter.ai/keys
squeezy

# Or use any other supported provider. See
# crates/squeezy-skills/external-docs/PROVIDERS.md for exact ids and env vars.
```

`squeezy doctor` reports on the merged configuration sources, repo profile,
configured provider credential, session-store path, and shell-sandbox tool
availability. See [`TROUBLESHOOTING.md`](crates/squeezy-skills/external-docs/TROUBLESHOOTING.md)
when startup looks wrong.

## License

See [LICENSE](LICENSE).

## Contributing

Build, test, clippy, and coverage commands are documented in
[CONTRIBUTING.md](CONTRIBUTING.md).
