# Release smoke tests

`scripts/local_release_smoke.sh` is the pre-flight check we run before
tagging a release. It exercises every channel listed in
[`release.yml`](../../.github/workflows/release.yml) against the
current commit, in isolated temp directories, without actually
publishing anything. Failures surface locally instead of mid-publish.

## What it checks

| Channel     | Exercised path                                                                                                       | What "pass" means                                                                                                                                              |
| ----------- | -------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `cargo`     | `cargo publish --dry-run --no-verify --allow-dirty -p <crate>` for every publishable workspace member                | Every publishable crate's manifest passes cargo's prepublish checks (version, included files, lockfile sanity).                                                |
| `installsh` | `install.sh` pointed at a `file://` mirror staged from a stub `squeezy` binary                                       | install.sh resolves the tag, downloads via `file://`, validates the sha256, extracts the archive, and lands an executable at `$SQUEEZY_INSTALL_DIR/squeezy`.   |
| `homebrew`  | `scripts/update_homebrew_formula.sh <tag> <assets> <tap>` against staged stub archives, then field + `ruby -c` check | The Homebrew formula is generated with the expected class, version, `on_macos` / `on_linux` blocks, sha256 lines, and `bin.install`, and parses as valid Ruby. |
| `winget`    | `scripts/update_winget_manifest.sh <tag> <assets> <fork>` against staged stub archives, then YAML structure check    | All three winget manifest YAMLs are written, reference the correct `PackageVersion`, and load via `yaml.safe_load` when PyYAML is available.                   |

## Usage

```bash
scripts/local_release_smoke.sh                  # all channels, tag derived from Cargo.toml
scripts/local_release_smoke.sh --only cargo     # one channel
scripts/local_release_smoke.sh --skip cargo     # skip the slow channel
scripts/local_release_smoke.sh --tag v1.2.3     # override the release tag
scripts/local_release_smoke.sh --keep-tmpdir    # keep the tmp tree on success
scripts/local_release_smoke.sh --verbose        # stream per-channel logs to stdout
```

The script:

- **Never publishes or pushes, and keeps release-channel outputs inside
  `$TMPDIR`.** Cargo uses `--dry-run` and writes build artifacts through
  `CARGO_TARGET_DIR` under the tmp tree, though it may still consult or update
  the normal Cargo registry cache. Homebrew/winget generators run against stub
  assets staged into the tmp tree. install.sh runs against a `file://` mirror
  under the same tmp tree.
- Creates a fresh `${TMPDIR:-/tmp}/squeezy-release-smoke.XXXXXX/` per
  invocation. Each channel gets its own subdirectory and a per-channel
  log under `logs/<channel>.log`.
- Aggregates pass/fail per channel and exits non-zero if anything
  failed. On failure the tmpdir is preserved automatically so the logs
  can be inspected.
- Derives the release tag from `[workspace.package].version` in
  `Cargo.toml` unless `--tag` is supplied. `release.yml` requires the
  workspace version to match the tag, so the default is the same shape
  CI will validate.

## When to run

- Before tagging a release locally. A clean run on the release-bump
  commit is the recommended gate.
- After touching `install.sh`, `scripts/update_homebrew_formula.sh`,
  `scripts/update_winget_manifest.sh`, or any release-channel wiring in
  `Cargo.toml` (workspace members, `publish` flags, version bumps).
- Optionally as a manual `workflow_dispatch` step before pushing a
  release tag — though the canonical CI gate is still `release.yml`
  itself.

## Output

A passing run looks like:

```
=== Squeezy local release smoke ===
repo root: /Users/.../squeezy
tag:       v0.1.0
tmpdir:    /tmp/squeezy-release-smoke.abc123

[cargo] starting...
[cargo] PASS (87s) -> /tmp/squeezy-release-smoke.abc123/logs/cargo.log
[installsh] starting...
[installsh] PASS (1s) -> /tmp/squeezy-release-smoke.abc123/logs/installsh.log
[homebrew] starting...
[homebrew] PASS (1s) -> /tmp/squeezy-release-smoke.abc123/logs/homebrew.log
[winget] starting...
[winget] PASS (1s) -> /tmp/squeezy-release-smoke.abc123/logs/winget.log

Summary
  channel    status  elapsed  log
  cargo      PASS      87s   /tmp/squeezy-release-smoke.abc123/logs/cargo.log
  installsh  PASS       1s   /tmp/squeezy-release-smoke.abc123/logs/installsh.log
  homebrew   PASS       1s   /tmp/squeezy-release-smoke.abc123/logs/homebrew.log
  winget     PASS       1s   /tmp/squeezy-release-smoke.abc123/logs/winget.log

All 4 channel(s) passed.
```

On failure, the summary marks the failing channel `FAIL`, the script
exits with status 1, the tmpdir is preserved, and the last 30 lines of
each failed log are echoed to stdout.

## Channel notes

### `cargo`

- Uses `--no-verify` because the registry-side compile step would
  otherwise need every workspace-internal dependency
  (`squeezy-agent`, `squeezy-core`, …) to already be published on
  crates.io. Compile health is covered by `cargo test --workspace` in
  CI; this smoke only validates the prepublish manifests.
- Uses `--allow-dirty` so the script can run from a working tree where
  the release-bump commit is staged but not yet committed.
- Sets `CARGO_TARGET_DIR` to a path inside the tmpdir so the run
  leaves no build artifacts behind in the developer's `target/`.
- Iterates every publishable workspace member discovered under
  `crates/*/Cargo.toml`. Crates marked `publish = false` are skipped.

Each publishable crate ends up in one of three buckets:

- **PASS** — `cargo publish --dry-run` succeeded outright. The crate's
  manifest is publish-ready and every dependency (including
  workspace-internal ones) is already discoverable on crates.io.
- **PENDING** — the only error was `no matching package named
  'squeezy-…' found`. The manifest itself is healthy; the crate would
  publish successfully once its workspace-internal dependencies land
  on crates.io. This is the expected state for every non-leaf crate
  before the *first* release because cargo can't know about a crate
  that hasn't been pushed to the index yet. Publishing in topological
  order (leaves first) resolves it.
- **FAIL** — any other cargo error: missing required fields in
  `Cargo.toml`, version mismatch, missing files, broken `[package]`
  inheritance from `[workspace.package]`, license metadata problems,
  etc. These block publishing and the channel reports failure.

### `installsh`

- Builds a tiny stub `squeezy` shell script (responds to `--version`
  and `doctor`), tar-gzips it as `squeezy-<host-target>.tar.gz`,
  drops the matching `.sha256` sidecar, and stages both under
  `tmpdir/installsh/mirror/<tag>/`.
- Runs `install.sh` with `SQUEEZY_INSTALL_BASE_URL=file://…`,
  `SQUEEZY_INSTALL_TAG=<tag>`,
  `SQUEEZY_INSTALL_DIR=tmpdir/installsh/installdir`, and a deliberately
  non-existent `SQUEEZY_GPG_KEY_PATH` so the optional GPG step is
  exercised as "skipped" rather than tripping over a locally installed
  release key.
- Pass requires the resulting `installdir/squeezy` to be executable
  and to produce a `--version` line.
- Only supports running on macOS or Linux (matching the host targets
  install.sh itself supports).

### `homebrew`

- Stages stub `.tar.gz` archives for all four release targets the
  formula references (arm64/x86_64 macOS, arm64/x86_64 musl Linux)
  plus the matching `.sha256` files.
- Runs `scripts/update_homebrew_formula.sh <tag> <assets> <tap>` and
  asserts the generated `Formula/squeezy.rb`:
  - contains `class Squeezy < Formula`, the correct `version "X.Y.Z"`,
    `on_macos` / `on_linux` blocks, at least one `sha256` line, and
    the `bin.install "squeezy"` step;
  - parses as valid Ruby (`ruby -c`) when Ruby is available.

> Note: this is intentionally not a `brew install` test. `brew install`
> would try to fetch the archives from the real GitHub release URLs,
> which only exist after publishing.

### `winget`

- Stages stub archives (including a placeholder Windows `.zip` — the
  generator only reads the sha256 sidecar) and runs
  `scripts/update_winget_manifest.sh <tag> <assets> <fork>`.
- Asserts the three required manifest YAMLs exist under
  `manifests/e/esqueezy/Squeezy/<version>/`, each references
  `PackageVersion: <version>`, and parses cleanly via
  `yaml.safe_load` when PyYAML is installed.

> Note: this is not a `winget install` test. Real winget validation
> requires Windows and access to the published release assets.

## Limitations

- Network-light but not network-free. `cargo publish --dry-run`
  contacts the crates.io index to confirm the candidate version isn't
  already published; if you are fully offline, the cargo channel will
  fail with a registry error.
- No end-to-end Homebrew or winget install. Those require real release
  artifacts published to GitHub; the smoke verifies the generators
  emit well-formed input for those tools, not that the tools succeed
  end-to-end.
- No real distribution archive is built. Each channel uses a tiny stub
  binary so the smoke stays seconds-fast. Cross-target binary builds
  are exercised by the `build-assets` job in `release.yml`.

## Extending the smoke when channels change

If a new release channel lands in `.github/workflows/release.yml`:

1. Add a `smoke_<channel>` function in
   `scripts/local_release_smoke.sh` that runs the channel's
   prepublish step against staged assets in
   `tmpdir/<channel>/`. Treat the tmpdir as the only place the
   channel may write.
2. Register the channel name in the `all_channels=(…)` array near
   the top of the script and add the matching `case` branch in the
   main dispatch loop.
3. Update the channel table and the per-channel notes section above
   so the doc and script stay in sync.
4. Confirm `scripts/local_release_smoke.sh --only <new-channel>` runs
   green on the current commit.
