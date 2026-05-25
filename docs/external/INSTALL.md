# Installation

Squeezy v0 supports macOS and Linux. The fastest path on any supported
platform is the one-line installer; Homebrew, Cargo, and GitHub release
archives are also supported.

## One-line installer

```sh
curl -fsSL https://raw.githubusercontent.com/esqueezy/squeezy/main/install.sh | sh
```

The installer detects your platform, downloads the matching tagged release
archive plus its SHA-256 sidecar, verifies the checksum, and installs the
`squeezy` binary into `$HOME/.local/bin` (override with
`SQUEEZY_INSTALL_DIR`). If that directory is not on your `PATH`, the
installer prints the line to add. Pin a specific release with
`SQUEEZY_INSTALL_TAG=v0.1.2`.

## Homebrew

Install from the Squeezy tap:

```sh
brew tap esqueezy/tap
brew install squeezy
squeezy --health
```

The one-command form is equivalent:

```sh
brew install esqueezy/tap/squeezy
```

Homebrew installs the matching macOS archive for Apple Silicon or Intel. The
formula smoke test runs `squeezy --health`.

## Cargo

Install from crates.io:

```sh
cargo install squeezy --locked
squeezy --health
```

Use this path when you already have a recent Rust toolchain installed. Squeezy
requires Rust 1.93.1 or newer.

## GitHub Release Archives

Tagged releases publish prebuilt archives and SHA-256 checksum files:

- `squeezy-aarch64-apple-darwin.tar.gz` for Apple Silicon macOS
- `squeezy-x86_64-apple-darwin.tar.gz` for Intel macOS
- `squeezy-x86_64-unknown-linux-musl.tar.gz` for Linux x86_64

Download the archive for your platform from
`https://github.com/esqueezy/squeezy/releases`, verify the checksum, then put
the `squeezy` binary on your `PATH`:

```sh
shasum -a 256 -c squeezy-aarch64-apple-darwin.tar.gz.sha256
tar -xzf squeezy-aarch64-apple-darwin.tar.gz
install -m 0755 squeezy /usr/local/bin/squeezy
squeezy --health
```

Replace the archive name with the Intel macOS or Linux archive when needed.

## First Run

For the default OpenAI provider, install, set your API key, initialize user
settings, then start a turn:

```sh
export OPENAI_API_KEY=...
squeezy config init --user
squeezy --health
squeezy --prompt "Reply with exactly: squeezy-ok"
```

The same install can open the TUI with:

```sh
squeezy
```

Provider and model choices can be changed with `SQUEEZY_PROVIDER`,
`SQUEEZY_MODEL`, CLI flags, or `~/.squeezy/settings.toml`. See
[`PROVIDERS.md`](PROVIDERS.md) and [`CONFIGURATION.md`](CONFIGURATION.md).

## Upgrade

```sh
brew update && brew upgrade squeezy
cargo install squeezy --locked --force
```

For GitHub release archives, download the newer archive and replace the binary
on your `PATH`.

## Uninstall

Homebrew:

```sh
brew uninstall squeezy
brew untap esqueezy/tap
```

Cargo:

```sh
cargo uninstall squeezy
```

Manual archive install:

```sh
rm /usr/local/bin/squeezy
```

Squeezy stores user settings and local runtime state under `~/.squeezy`. Remove
that directory only if you also want to delete settings, sessions, caches,
reports, and local repo profiles:

```sh
rm -rf ~/.squeezy
```
