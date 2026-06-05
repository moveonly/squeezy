# Squeezy

A coding agent that treats cost, speed, and code understanding as first-class
citizens. Squeezy parses repositories into a persistent local semantic graph and
queries that graph through structured tools that return compact evidence
packets — spans, hashes, confidence, freshness — instead of raw file dumps.

> **Status:** early access. The CLI/TUI is runnable and installable, with
> provider presets, a persistent semantic graph, release automation, and an
> evaluation harness. Graph-backed navigation, deterministic validation
> harnesses, local help, sessions/resume, checkpoints, provider routing, MCP,
> skills, hooks, feedback/reporting, and telemetry are implemented and still
> evolving, so expect rough edges. Provider and language support
> change faster than this README, so use
> [`PROVIDERS.md`](crates/squeezy-skills/external-docs/PROVIDERS.md) for exact
> provider ids, defaults, environment variables, and model metadata, and
> [`LANGUAGES.md`](crates/squeezy-skills/external-docs/LANGUAGES.md) for the
> current graph-navigation coverage matrix.

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
brew install squeezy
```

If the core formula is not available on your machine yet, use
`brew install esqueezy/tap/squeezy`.

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

# Aggregator path: one key can route to many upstream models.
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
