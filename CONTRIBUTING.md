# Contributing

Squeezy is implemented in Rust and targets macOS, Linux, and Windows (x86_64). The foundation workspace uses Rust 2024.

Contributor-facing architecture, validation, benchmark, and deployment notes
live in [`docs/internal/`](docs/internal/). User-facing product docs live in
[`crates/squeezy-skills/external-docs/`](crates/squeezy-skills/external-docs/)
(co-located with the crate that bundles them into the binary at build time)
and are embedded into built-in Squeezy help, so update them whenever
user-visible behavior changes.

## Setup

Install Rust `1.93.1` or newer. The repository pins `1.93.1` in `rust-toolchain.toml` and each crate inherits `rust-version = "1.93.1"` from the workspace.

Install pre-commit hooks:

```sh
brew install pre-commit gitleaks actionlint cargo-deny typos-cli
pre-commit install
```

If you do not use Homebrew, install `pre-commit`, `gitleaks`, `actionlint`, `cargo-deny`, and `typos` with your platform's package manager.

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

Build release binaries:

```sh
cargo build --release -p squeezy
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo build --release -p squeezy --target x86_64-unknown-linux-musl
```

The musl release build is the Linux distribution artifact. `musl-gcc` is used for native C dependencies, while `rust-lld` links the final self-contained Rust artifact. Verify that the binary has no dynamic loader and no shared-library dependencies:

```sh
if readelf -l target/x86_64-unknown-linux-musl/release/squeezy | grep -q INTERP; then
  echo "unexpected dynamic interpreter"
  exit 1
fi

if readelf -d target/x86_64-unknown-linux-musl/release/squeezy 2>/dev/null | grep -q NEEDED; then
  echo "unexpected shared-library dependency"
  exit 1
fi
```

Both checks should pass without printing an error.

## Test

```sh
cargo test --workspace --all-targets
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo test --workspace --all-targets --target x86_64-unknown-linux-musl
cargo run -p squeezy-harness -- run --jsonl target/harness.jsonl
python3 scripts/check_test_layout.py
```

Rust's stable test runner does not have first-class test tags. Costly integration tests use the `costly` name/ignore convention, a Cargo feature for explicit opt-in, and environment checks for required secrets. They are ignored by default and must be run explicitly:

```sh
SQUEEZY_RUN_COSTLY_TESTS=1 \
OPENAI_API_KEY=... \
cargo test -p squeezy-llm --features costly-tests --test openai_costly -- --ignored
```

Run the Anthropic costly smoke test with:

```sh
SQUEEZY_RUN_COSTLY_TESTS=1 \
ANTHROPIC_API_KEY=... \
cargo test -p squeezy-llm --features costly-tests --test anthropic_costly -- --ignored
```

Use `SQUEEZY_COSTLY_OPENAI_MODEL` or `SQUEEZY_COSTLY_ANTHROPIC_MODEL` to test a different cheap model for one provider. `SQUEEZY_COSTLY_MODEL` is the shared fallback. The default costly OpenAI model is `gpt-5-nano`; the default costly Anthropic model is `claude-3-5-haiku-20241022`. Use `SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS=256` if a smoke run is truncated by the provider before returning the expected text.

## Clippy and Formatting

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-musl -- -D warnings
pre-commit run --all-files
gitleaks detect --source . --redact --no-banner --no-color --verbose
actionlint
typos README.md CONTRIBUTING.md docs .github
cargo deny check
cargo shear
```

## Coverage

```sh
cargo llvm-cov --workspace --all-targets --summary-only
CC_x86_64_unknown_linux_musl=musl-gcc \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
cargo llvm-cov --workspace --all-targets --target x86_64-unknown-linux-musl --summary-only
```

For an HTML report:

```sh
cargo llvm-cov --workspace --all-targets --html
```

On every pull request and every push to `main`, CI runs:

- secret scanning (`gitleaks`)
- workflow linting (`actionlint`)
- dependency policy (`cargo deny check --all-features`)
- dead dependency scan (`cargo shear`)
- docs text linting (`typos`)
- formatting (`cargo fmt --all -- --check`)
- unit test layout (`scripts/check_test_layout.py`)
- clippy (`cargo clippy --workspace --all-targets -- -D warnings`)
- tests (`cargo test --workspace --all-targets`)
- deterministic validation harness runners (`squeezy-harness`)
- coverage summary (push-to-main and manual dispatch only)
- debug artifact build and smoke-test

The dependency policy in `deny.toml` covers RustSec advisories, duplicate dependencies, license allow-lists, and registry/git source policy for the macOS targets and the Linux musl release target. The Linux job runs clippy, tests, harness validation, coverage, and artifact packaging against `x86_64-unknown-linux-musl`. The coverage step writes its text summary to the GitHub job summary.

Pushing a `v*` tag runs the release workflow. It builds and smoke-tests downloadable archives for `x86_64-apple-darwin`, `aarch64-apple-darwin`, and `x86_64-unknown-linux-musl`, uploads checksum files, and publishes a GitHub Release with generated notes. Dependabot tracks Cargo workspace dependencies, benchmark harness dependencies, and GitHub Actions updates weekly.

When `HOMEBREW_TAP_TOKEN` is configured for the repository, the release workflow
also updates `esqueezy/homebrew-tap` from the published archive checksums. The
tap update is generated by `scripts/update_homebrew_formula.sh`.

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
