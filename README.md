# Squeezy

A coding agent that treats cost, speed, and code understanding as first-class
citizens. Squeezy parses repositories into a persistent local semantic graph and
queries that graph through structured tools that return compact evidence
packets — spans, hashes, confidence, freshness — instead of raw file dumps.

> **Status:** early development. The TUI scaffold is runnable; OpenAI,
> Anthropic, Gemini, Azure OpenAI, Ollama, and Bedrock adapters are available;
> deterministic validation harness tasks run in CI; graph-backed navigation
> tools expose compact evidence packets.

The **why** lives in [`docs/THESIS.md`](docs/THESIS.md). User docs live in
[`docs/external/`](docs/external) and contributor docs live in
[`docs/internal/`](docs/internal).

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

Tagged releases also publish macOS Intel, macOS Apple Silicon, and Linux
x86_64 musl archives. Full install, first-run, upgrade, and uninstall
instructions are in [`docs/external/INSTALL.md`](docs/external/INSTALL.md).

## Quickstart

```sh
squeezy doctor                    # diagnose configuration and providers
squeezy config init --user        # write the default user settings file
export OPENAI_API_KEY=...         # or pick another provider; bring your own key
squeezy                           # open the TUI
```

`squeezy doctor` reports on the merged configuration sources, repo profile,
configured provider credential, session-store path, and shell-sandbox tool
availability. See [`docs/external/TROUBLESHOOTING.md`](docs/external/TROUBLESHOOTING.md)
when startup looks wrong.

## License

See [LICENSE](LICENSE).

## Contributing

Build, test, clippy, and coverage commands are documented in
[CONTRIBUTING.md](CONTRIBUTING.md).
