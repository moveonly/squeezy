# Platform Support

Squeezy v0 supports macOS and Linux.

## macOS

macOS is built and tested in CI on GitHub-hosted macOS runners. Pull requests
and pushes to `main` build and upload a debug artifact with:

```sh
cargo build -p squeezy-cli
```

The full release artifact is built and uploaded only for pushes to `main` and
manual CI runs. It uses the workspace `release` profile:

```sh
cargo build --release -p squeezy-cli
```

CI smoke-tests both debug and release artifacts with:

```sh
squeezy --health
```

## Linux

Linux is built and tested in CI on GitHub-hosted Ubuntu runners. The distributable Linux artifact is built for:

```text
x86_64-unknown-linux-musl
```

That target is used so the Linux binary is statically linked and does not depend on glibc. This makes the artifact usable on Alpine and ordinary glibc-based distributions without shipping separate libc builds.

CI uses the musl target for Linux validation and artifact upload. The jobs set `CC_x86_64_unknown_linux_musl=musl-gcc` for native C dependencies and `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld` for the final target link.

- `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-musl -- -D warnings`
- `cargo test --workspace --all-targets --target x86_64-unknown-linux-musl`
- `cargo build -p squeezy-cli --target x86_64-unknown-linux-musl`
- `cargo llvm-cov --workspace --all-targets --target x86_64-unknown-linux-musl --summary-only` for coverage on pushes to `main` and manual CI runs
- `cargo build --release -p squeezy-cli --target x86_64-unknown-linux-musl` for pushes to `main` and manual CI runs
- `readelf -l` must not report a program interpreter.
- `readelf -d` must not report dynamic `NEEDED` dependencies.
- the binary must pass `--health`, `--version`, and `--help`.

Manual CI runs accept an optional `checkout_ref` input for building a branch,
tag, SHA, or pull request ref such as `refs/pull/123/merge`.

## Not Yet Supported

Windows is not part of v0 support. Windows support should be added deliberately because shell execution, paths, terminal raw mode, CTRL-C behavior, executable lookup, and future PTY behavior need separate test coverage.
