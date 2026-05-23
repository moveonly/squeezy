# Platform Support

Squeezy v0 supports macOS and Linux.

## macOS

macOS is built and tested in CI on GitHub-hosted macOS runners. The release artifact is built with:

```sh
cargo build --release -p squeezy-cli
```

CI smoke-tests the release binary with:

```sh
target/release/squeezy --health
```

## Linux

Linux is built and tested in CI on GitHub-hosted Ubuntu runners. The distributable Linux artifact is built for:

```text
x86_64-unknown-linux-musl
```

That target is used so the Linux binary is statically linked and does not depend on glibc. This makes the artifact usable on Alpine and ordinary glibc-based distributions without shipping separate libc builds.

CI uses the musl target for Linux validation and artifact upload. The job sets `CC_x86_64_unknown_linux_musl=musl-gcc` for native C dependencies and `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld` for the final target link.

- `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-musl -- -D warnings`
- `cargo test --workspace --all-targets --target x86_64-unknown-linux-musl`
- `cargo llvm-cov --workspace --all-targets --target x86_64-unknown-linux-musl --summary-only`
- `cargo build --release -p squeezy-cli --target x86_64-unknown-linux-musl`
- `readelf -l` must not report a program interpreter.
- `readelf -d` must not report dynamic `NEEDED` dependencies.
- the binary must pass `--health`, `--version`, and `--help`.

## Not Yet Supported

Windows is not part of v0 support. Windows support should be added deliberately because shell execution, paths, terminal raw mode, CTRL-C behavior, executable lookup, and future PTY behavior need separate test coverage.
