# Installation

Squeezy v0 supports macOS, Linux, and Windows (x86_64). On macOS and
Linux the fastest path is the one-line installer; on Windows it is
Winget. Homebrew, Cargo, and direct GitHub release archives work on every
platform.

## One-line installer (macOS and Linux)

```sh
curl -fsSL https://raw.githubusercontent.com/esqueezy/squeezy/main/install.sh | sh
```

The installer detects your platform, downloads the matching tagged release
archive plus its SHA-256 sidecar, verifies the checksum, and installs the
`squeezy` binary into `$HOME/.local/bin` (override with
`SQUEEZY_INSTALL_DIR`). If that directory is not on your `PATH`, the
installer prints the line to add. Pin a specific release with
`SQUEEZY_INSTALL_TAG=v0.1.2`. The script is POSIX-shell only; Windows
users should use Winget or the manual zip install.

## Winget (Windows)

```powershell
winget install esqueezy.Squeezy
squeezy doctor
```

Winget installs the Windows x86_64 archive into the per-user portable apps
directory and creates a `squeezy` command alias on PATH.

## Homebrew

Install from the Squeezy tap:

```sh
brew tap esqueezy/tap
brew install squeezy
squeezy doctor
```

The one-command form is equivalent:

```sh
brew install esqueezy/tap/squeezy
```

Homebrew installs the matching macOS archive for Apple Silicon or Intel. The
formula smoke test runs `squeezy doctor`.

## Cargo

Install from crates.io:

```sh
cargo install squeezy --locked
squeezy doctor
```

Use this path when you already have a recent Rust toolchain installed. Squeezy
requires Rust 1.93.1 or newer.

For local source checkouts, keep install artifacts in a persistent target
directory so repeat installs can reuse them:

```sh
cargo install --path crates/squeezy-cli --locked --target-dir target/local-install
```

Add `--timings` when investigating slow installs; Cargo writes an HTML report
under the target directory.

## GitHub Release Archives

Tagged releases publish prebuilt archives and SHA-256 checksum files:

- `squeezy-aarch64-apple-darwin.tar.gz` for Apple Silicon macOS
- `squeezy-x86_64-apple-darwin.tar.gz` for Intel macOS
- `squeezy-x86_64-unknown-linux-musl.tar.gz` for Linux x86_64
- `squeezy-aarch64-unknown-linux-musl.tar.gz` for Linux ARM64
- `squeezy-x86_64-pc-windows-msvc.zip` for Windows x86_64

Download the archive for your platform from
`https://github.com/esqueezy/squeezy/releases`, verify the checksum, then put
the `squeezy` binary on your `PATH`:

```sh
shasum -a 256 -c squeezy-aarch64-apple-darwin.tar.gz.sha256
tar -xzf squeezy-aarch64-apple-darwin.tar.gz
install -m 0755 squeezy /usr/local/bin/squeezy
squeezy doctor
```

Replace the archive name with the Intel macOS, Linux x86_64, or Linux ARM64
archive when needed.

On Windows, expand the zip and add the install location to `PATH`:

```powershell
Expand-Archive -Path squeezy-x86_64-pc-windows-msvc.zip `
  -DestinationPath $env:LOCALAPPDATA\Programs\squeezy
[Environment]::SetEnvironmentVariable(
  "Path",
  "$env:Path;$env:LOCALAPPDATA\Programs\squeezy",
  "User"
)
squeezy doctor
```

The Windows release archive is currently unsigned. Windows SmartScreen may
display a "Windows protected your PC" warning the first time the binary is
launched; click "More info" → "Run anyway" once and the prompt won't
repeat for that binary. Code-signing the release artifact is on the
roadmap.

## First Run

For the default OpenAI provider, install, set your API key, initialize user
settings, then start a turn:

```sh
export OPENAI_API_KEY=...
squeezy config init --user
squeezy doctor
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

```powershell
winget upgrade esqueezy.Squeezy
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

Winget:

```powershell
winget uninstall esqueezy.Squeezy
```

Manual archive install:

```sh
rm /usr/local/bin/squeezy
```

```powershell
Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\squeezy"
```

Squeezy stores user settings and local runtime state under `~/.squeezy` on
Unix and `%APPDATA%\squeezy` on Windows. Remove that directory only if you
also want to delete settings, sessions, caches, reports, and local repo
profiles:

```sh
rm -rf ~/.squeezy
```

```powershell
Remove-Item -Recurse -Force "$env:APPDATA\squeezy"
```
