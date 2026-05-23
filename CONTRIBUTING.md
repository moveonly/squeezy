# Contributing

Squeezy is implemented in Rust and targets macOS first. The foundation workspace uses Rust 2024.

## Setup

Install Rust `1.93.1` or newer. The repository pins `1.93.1` in `rust-toolchain.toml` and each crate inherits `rust-version = "1.93.1"` from the workspace.

Install pre-commit hooks:

```sh
brew install pre-commit gitleaks actionlint cargo-deny
pre-commit install
```

If you do not use Homebrew, install `pre-commit`, `gitleaks`, `actionlint`, and `cargo-deny` with your platform's package manager.

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

## Test

```sh
cargo test --workspace --all-targets
python3 scripts/check_test_layout.py
```

Rust's stable test runner does not have first-class test tags. Costly integration tests use the `costly` name/ignore convention, a Cargo feature for explicit opt-in, and environment checks for required secrets. They are ignored by default and must be run explicitly:

```sh
SQUEEZY_RUN_COSTLY_TESTS=1 \
OPENAI_API_KEY=... \
cargo test -p squeezy-llm --features costly-tests --test openai_costly -- --ignored
```

Use `SQUEEZY_COSTLY_MODEL=gpt-5-mini` to test a different cheap model. The default costly model is `gpt-5-nano`. Use `SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS=256` if a smoke run is truncated by the provider before returning the expected text.

## Clippy and Formatting

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
pre-commit run --all-files
gitleaks detect --source . --redact --no-banner --no-color --verbose
actionlint
cargo deny check
```

## Coverage

```sh
cargo llvm-cov --workspace --all-targets --summary-only
```

For an HTML report:

```sh
cargo llvm-cov --workspace --all-targets --html
```

CI runs secret scanning, workflow linting, dependency policy checks, formatting, test-layout, clippy, tests, and coverage checks on every pull request and every push to `main`. The dependency policy in `deny.toml` covers RustSec advisories, duplicate dependencies, license allow-lists, and registry/git source policy. The coverage step writes its text summary to the GitHub job summary.

## Run

Open the TUI:

```sh
cargo run -p squeezy-cli
```

Use a specific OpenAI model:

```sh
SQUEEZY_MODEL=gpt-5-mini cargo run -p squeezy-cli
```

Set `OPENAI_API_KEY` to stream real model responses. Without it, the TUI still starts and reports a provider configuration error when a turn is submitted.

Run a cheap non-interactive smoke command manually:

```sh
OPENAI_API_KEY=... cargo run -p squeezy-cli -- \
  --model gpt-5-nano \
  --max-output-tokens 32 \
  --prompt "Reply with exactly: squeezy-ok"
```

Do not run costly provider smoke checks from AI automation. Use the ignored `costly` integration test locally with `--features costly-tests` when you explicitly want to spend API tokens.

## Unit Test Layout

Keep unit tests in sibling files. See `docs/TEST_LAYOUT.md`.
