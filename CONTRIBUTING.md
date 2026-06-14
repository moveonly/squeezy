# Contributing

Squeezy is implemented in Rust and targets macOS, Linux, and Windows (x86_64). The foundation workspace uses Rust 2024.

Contributor-facing architecture, validation, benchmark, and deployment notes
live in [`docs/internal/`](docs/internal/). User-facing product docs live in
[`crates/squeezy-skills/external-docs/`](crates/squeezy-skills/external-docs/)
(co-located with the crate that bundles them into the binary at build time)
and are embedded into built-in Squeezy help, so update them whenever
user-visible behavior changes.

## Setup

Install Rust `1.96.0` or newer. The repository pins `1.96.0` in `rust-toolchain.toml` and each crate inherits `rust-version = "1.96.0"` from the workspace.

Install pre-commit hooks:

```sh
brew install pre-commit gitleaks actionlint cargo-deny typos-cli
cargo install cargo-shear cargo-llvm-cov --locked
pre-commit install
```

If you do not use Homebrew, install `pre-commit`, `gitleaks`, `actionlint`,
`cargo-deny`, and `typos` with your platform's package manager, and install
`cargo-shear` / `cargo-llvm-cov` with Cargo or a trusted binary installer.

On Debian/Ubuntu Linux, install the packages needed for the static musl release build:

```sh
sudo apt-get install musl-tools file binutils
rustup target add x86_64-unknown-linux-musl
```

On Windows (x86_64), install Visual Studio Build Tools with the "Desktop
development with C++" workload (required for the MSVC linker) and Rust
via rustup. `cargo nextest`, `cargo deny`, and `cargo clippy` work
unchanged. `actionlint`, `typos`, `gitleaks`, the coverage step, and the
`install.sh` smoke test only run on the macOS CI matrix entry. The Linux
`musl-tools`, `readelf`, and `file` invariants do not apply. Run shell
commands through Git Bash (`shell: bash` in CI) or PowerShell.

For coverage, install `cargo-llvm-cov`:

```sh
cargo install cargo-llvm-cov --locked
```

If your local Rust compiler is older than the newest `cargo-llvm-cov` release supports, install the latest compatible version with `--locked`.

If you use Homebrew Rust instead of `rustup`, expose Homebrew LLVM tools when running coverage:

```sh
LLVM_COV=/opt/homebrew/opt/llvm/bin/llvm-cov \
LLVM_PROFDATA=/opt/homebrew/opt/llvm/bin/llvm-profdata \
cargo llvm-cov --workspace --all-targets --summary-only
```

## Build

```sh
cargo build --workspace --all-targets
```

Build release-parity binaries:

```sh
cargo build --profile dist -p squeezy
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo build --profile dist -p squeezy --target x86_64-unknown-linux-musl
```

The `dist` profile is the release workflow profile: it inherits release
optimization and enables LTO, one codegen unit, and symbol stripping. The musl
build is the Linux distribution artifact. `musl-gcc` is used for native C
dependencies, while `rust-lld` links the final self-contained Rust artifact.
Verify that the binary has no dynamic loader and no shared-library
dependencies:

```sh
if readelf -l target/x86_64-unknown-linux-musl/dist/squeezy | grep -q INTERP; then
  echo "unexpected dynamic interpreter"
  exit 1
fi

if readelf -d target/x86_64-unknown-linux-musl/dist/squeezy 2>/dev/null | grep -q NEEDED; then
  echo "unexpected shared-library dependency"
  exit 1
fi
```

Both checks should pass without printing an error.

## Test

```sh
scripts/test_clean_env.sh --workspace --all-targets --profile ci
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
scripts/test_clean_env.sh --workspace --all-targets --profile ci --target x86_64-unknown-linux-musl
cargo run -p squeezy-harness -- run --jsonl target/harness.jsonl
python3 scripts/check_test_layout.py
```

Rust's stable test runner does not have first-class test tags. Costly provider
integration tests live under `crates/squeezy-llm/tests/*_costly.rs` and use the
`costly` name/ignore convention, the `costly-tests` Cargo feature, the
`SQUEEZY_RUN_COSTLY_TESTS=1` master flag, and provider-specific environment
checks. They are ignored by default and must be run explicitly.

Example OpenAI smoke:

```sh
SQUEEZY_RUN_COSTLY_TESTS=1 \
OPENAI_API_KEY=... \
cargo test -p squeezy-llm --features costly-tests --test openai_costly -- --ignored
```

Example Anthropic smoke:

```sh
SQUEEZY_RUN_COSTLY_TESTS=1 \
ANTHROPIC_API_KEY=... \
cargo test -p squeezy-llm --features costly-tests --test anthropic_costly -- --ignored
```

Other costly test binaries document their required credentials in the
`#[ignore = "costly: ..."]` reason. Many support a
`SQUEEZY_COSTLY_<PROVIDER>_MODEL` override. OpenAI reads the shared
`SQUEEZY_COSTLY_MODEL` override; Anthropic reads
`SQUEEZY_COSTLY_ANTHROPIC_MODEL` first and then falls back to
`SQUEEZY_COSTLY_MODEL`. Use `SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS=256` if a smoke
run is truncated by the provider before returning the expected text.

For non-costly test runs where you want a hard guarantee that no paid provider call can fire, invoke nextest through the clean-env wrapper:

```sh
scripts/test_clean_env.sh --workspace --all-targets
```

The wrapper unsets every vendor API key, every `SQUEEZY_<PROVIDER>_KEY` fallback, the `SQUEEZY_RUN_COSTLY_TESTS` master flag, and the `SQUEEZY_CREDENTIALS_JSON` / `SQUEEZY_CREDENTIALS_FILE` aggregate channels before forwarding the remaining arguments to `cargo nextest run`. CI uses it as the default test invocation so a misconfigured runner or a stray `export` in a debug step cannot accidentally bill a real provider; use it locally before merging changes to any `*_costly.rs` file to confirm the gates still skip cleanly. The list of stripped variables lives in the script itself — keep it in sync when adding a new provider.

## Clippy and Formatting

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --no-deps -- -D warnings
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo clippy --workspace --all-targets --no-deps --target x86_64-unknown-linux-musl -- -D warnings
pre-commit run --all-files
gitleaks detect --source . --redact --no-banner --no-color --verbose
actionlint
typos README.md CONTRIBUTING.md docs .github
cargo deny check
cargo shear
```

## Coverage

```sh
cargo llvm-cov nextest --workspace --all-targets --profile ci --summary-only
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo llvm-cov nextest --workspace --all-targets --profile ci --target x86_64-unknown-linux-musl --summary-only
```

For an HTML report:

```sh
cargo llvm-cov nextest --workspace --all-targets --profile ci --html
```

On every pull request and every push to `main`, CI runs:

- secret scanning (`gitleaks`)
- workflow linting (`actionlint`)
- dependency policy (`cargo deny check --all-features`)
- dead dependency scan (`cargo shear`)
- build-script allowlist (`scripts/check_build_scripts.py`)
- docs text linting (`typos`)
- formatting (`cargo fmt --all -- --check`)
- unit test layout (`scripts/check_test_layout.py`)
- clippy (`cargo clippy --workspace --all-targets --no-deps -- -D warnings`)
- tests through `scripts/test_clean_env.sh --workspace --all-targets --profile ci`
- deterministic validation harness runners (`squeezy-harness`)
- coverage summary (pull requests, push-to-main, and manual dispatch)
- debug artifact build and smoke-test

The dependency policy in `deny.toml` covers RustSec advisories, duplicate
dependencies, license allow-lists, and registry/git source policy. The Linux CI
path runs clippy, tests, harness validation, debug artifact smoke checks, and
coverage against `x86_64-unknown-linux-musl`. The coverage step writes its text
summary to the GitHub job summary.

Separately from the per-PR / per-release dependency policy job, the
`Scheduled advisory rescan` workflow (`.github/workflows/advisory-rescan.yml`)
runs `cargo deny --all-features check advisories` against the committed
`Cargo.lock` every day at 06:00 UTC (and on manual `workflow_dispatch`). When
RustSec publishes a new advisory that affects a pinned dependency, the
scheduled job opens a tracking issue labelled `advisory-rescan` (or comments on
the existing open one) with the cargo-deny output, the run URL, and the
`Cargo.lock` sha256. Resolve by upgrading the affected crate or by adding a
justified entry to `[advisories.ignore]` in `deny.toml`, then close the issue
after the next clean rescan.

Pushing a `v*` tag runs the release workflow. It builds and smoke-tests downloadable archives for `x86_64-apple-darwin`, `aarch64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, and `x86_64-pc-windows-msvc`, uploads checksum files, and publishes a GitHub Release with generated notes. Dependabot tracks Cargo workspace dependencies, benchmark harness dependencies, and GitHub Actions updates weekly.

When `HOMEBREW_TAP_TOKEN` is configured for the repository, the release workflow
also updates `esqueezy/homebrew-tap` from the published archive checksums. The
tap update is generated by `scripts/update_homebrew_formula.sh`.

When `WINGET_PKGS_TOKEN` is configured, the release workflow runs
`scripts/update_winget_manifest.sh` against the `esqueezy/winget-pkgs` fork.
The first manifest still needs a manual PR to `microsoft/winget-pkgs`;
subsequent releases can reuse the same branch/update path.

Before a crates.io release, list package contents for every publishable Squeezy
crate, then dry-run publish crates in dependency order. During the first
crates.io bootstrap, a dependent crate's dry-run can only resolve after its
internal dependencies already exist on crates.io, so run the dry-run, perform
the real `cargo publish`, and then move to the next package.

```sh
for package in \
  squeezy-core \
  squeezy-workspace \
  squeezy-vcs \
  squeezy-rank \
  squeezy-llm \
  squeezy-mcp \
  squeezy-skills \
  squeezy-hooks \
  squeezy-telemetry \
  squeezy-parse \
  squeezy-store \
  squeezy-graph \
  squeezy-tools \
  squeezy-agent \
  squeezy-tui \
  squeezy
do
  cargo package -p "$package" --list
  cargo publish -p "$package" --dry-run
done
```

## Run

Open the TUI:

```sh
cargo run -p squeezy
```

Use a specific OpenAI model:

```sh
SQUEEZY_MODEL=gpt-5-mini cargo run -p squeezy
```

Set `OPENAI_API_KEY` to stream real model responses. Without it, the TUI still starts and reports a provider configuration error when a turn is submitted.

Run a cheap non-interactive smoke command manually:

```sh
OPENAI_API_KEY=... cargo run -p squeezy -- \
  --model gpt-5-nano \
  --max-output-tokens 32 \
  --prompt "Reply with exactly: squeezy-ok"
```

Do not run costly provider smoke checks from AI automation. Use the ignored `costly` integration test locally with `--features costly-tests` when you explicitly want to spend API tokens.

## Unit Test Layout

Keep unit tests in sibling files. See `docs/internal/TEST_LAYOUT.md`.
