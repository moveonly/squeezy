# Squeezy

A coding agent that treats cost, speed, and code understanding as first-class
citizens. Squeezy parses repositories into a persistent local semantic graph and
queries that graph through structured tools that return compact evidence
packets — spans, hashes, confidence, freshness — instead of raw file dumps.

> **Status:** early development. The TUI scaffold is runnable; deterministic
> validation harness tasks run in CI; graph-backed navigation tools expose
> compact evidence packets. Providers:
>
> - **Aggregators (one key, many models):** OpenRouter, Vercel AI Gateway,
>   PortKey.
> - **First-party vendor APIs (single vendor):** OpenAI, Anthropic, Google
>   Gemini.
> - **Cloud-platform hosts:** Amazon Bedrock (AWS multi-vendor catalog), Azure
>   OpenAI (Microsoft-hosted OpenAI), Google Vertex AI (GCP-hosted Gemini and
>   partner models).
> - **Local runtime:** Ollama.
> - **Other OpenAI-compatible:** Groq, xAI, DeepSeek, Mistral La Plateforme,
>   Together AI, Fireworks AI, Cerebras.
> - Any other OpenAI-compatible endpoint — Microsoft Foundry (Azure AI Studio)
>   for the broader Foundry catalog, Cloudflare Workers AI, self-hosted
>   LiteLLM, … — works via the `openai_compatible` preset.

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
musl, and Windows x86_64 archives. Full install, first-run, upgrade, and
uninstall instructions are in [`INSTALL.md`](crates/squeezy-skills/external-docs/INSTALL.md).

## Quickstart

```sh
squeezy doctor                    # diagnose configuration and providers
squeezy config init --user        # write the default user settings file

# Fastest path: one credit, every frontier model (recommended)
export OPENROUTER_API_KEY=...     # https://openrouter.ai/keys
squeezy

# Or use any other supported provider — first-party vendor APIs (OpenAI,
# Anthropic, Google Gemini), cloud-platform hosts (Azure OpenAI, Amazon
# Bedrock), local Ollama, or other OpenAI-compatible services (Vercel AI
# Gateway, PortKey, Groq, xAI, DeepSeek, Mistral, Together AI, Fireworks AI,
# Cerebras). See crates/squeezy-skills/external-docs/PROVIDERS.md for the matching env vars.
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
