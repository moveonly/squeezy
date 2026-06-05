# Website Install and Distribution Research

Status: current-tree research for website copy. Last checked in this checkout on
2026-06-05. This is not the user-facing install reference; the bundled
user-facing source is currently
`crates/squeezy-skills/external-docs/INSTALL.md` and
`crates/squeezy-skills/external-docs/PLATFORMS.md`.

Keep public copy conservative. Prefer install-channel facts that are backed by
release workflow, script, or docs evidence. Do not imply that optional post-
release publishing jobs are guaranteed to run unless their required tokens are
configured.

## Evidence Checked

- `install.sh`: one-line installer behavior, target detection, checksum/GPG
  handling, and install directory defaults.
- `README.md`: top-level install summary and current public repo positioning.
- `crates/squeezy-skills/external-docs/INSTALL.md`: bundled user-facing install,
  upgrade, uninstall, and first-run docs.
- `crates/squeezy-skills/external-docs/PLATFORMS.md`: bundled platform support,
  CI/release validation, and Windows caveats.
- `squeezy-site/src/pages/install.astro`,
  `squeezy-site/src/pages/docs/install.astro`, and
  `squeezy-site/src/facts.ts`: current website install copy.
- `scripts/update_homebrew_formula.sh` and
  `scripts/update_winget_manifest.sh`: generated package metadata.
- `scripts/local_release_smoke.sh` and `docs/internal/RELEASE_SMOKE.md`: local
  preflight coverage and limitations.
- `.github/workflows/release.yml` and relevant `.github/workflows/ci.yml`
  snippets: actual release artifacts, smoke checks, and optional package updates.
- Root `Cargo.toml` and `crates/*/Cargo.toml`: package names, publish posture,
  Rust version, and binary naming.

Note: `docs/external/INSTALL.md` and `docs/external/PLATFORMS.md` are not present
in this checkout. The skill bundle maps files from
`crates/squeezy-skills/external-docs/` to logical `docs/external/...` paths at
build time.

## High-Level Finding

The install story is credible but still early-release shaped:

- Primary OS paths: curl installer for macOS/Linux, Winget for Windows,
  Homebrew for macOS and x86_64 Linux, Cargo for Rust users, and direct GitHub
  archives for all release targets.
- Release automation builds five archives: macOS Intel, macOS Apple Silicon,
  Linux x86_64 musl, Linux ARM64 musl, and Windows x86_64 MSVC.
- The website currently underreports Linux ARM64 in several places: release
  workflow and `install.sh` support `aarch64-unknown-linux-musl`, while site
  facts/docs currently mention only Linux x86_64.
- Homebrew formula generation only supports macOS Apple Silicon, macOS Intel,
  and Linux x86_64. It explicitly rejects non-Intel Linux.
- Windows is supported through x86_64 zip/Winget, but the archive is currently
  unsigned and SmartScreen warnings are expected.
- Homebrew and Winget updates are post-release jobs that can skip when their
  secret tokens are absent. Website copy should say "supported path" or
  "release automation can update" rather than "every release is immediately
  available" unless publish history proves it.

## Install Channel Matrix

| Channel | Public-safe claim | Evidence | Caveats |
| --- | --- | --- | --- |
| One-line installer | "Install on macOS and Linux with a curl installer that downloads the matching release archive, verifies SHA-256, and installs `squeezy` into `$HOME/.local/bin` by default." | `install.sh:4-27`, `install.sh:35-39`, `install.sh:79-100`, `install.sh:133-146`, `install.sh:172-186`; `crates/squeezy-skills/external-docs/INSTALL.md:8-20` | The script does not support Windows. It depends on `curl` or `wget`, `tar`, and `shasum` or `sha256sum`. GPG signature verification is optional and skipped if `gpg`, the public key, or a `.asc` file is unavailable. |
| GitHub release archives | "Tagged releases publish native archives with SHA-256 sidecars for macOS, Linux, and Windows." | `.github/workflows/release.yml:35-54`, `.github/workflows/release.yml:199-218`, `.github/workflows/release.yml:238-255`, `.github/workflows/release.yml:270-284`; `crates/squeezy-skills/external-docs/INSTALL.md:73-112` | Signature artifacts are uploaded only when `RELEASE_GPG_PRIVATE_KEY` is configured. Windows archive is unsigned today. |
| Homebrew | "Homebrew install is supported from the Squeezy tap for macOS, with x86_64 Linux support in the generated formula." | `README.md:39-43`; `crates/squeezy-skills/external-docs/INSTALL.md:32-50`; `scripts/update_homebrew_formula.sh:75-130`; `.github/workflows/release.yml:286-340` | Formula generation includes macOS arm64/x86_64 and Linux x86_64 only. Linux ARM64 is not supported by the formula. The release job skips if `HOMEBREW_TAP_TOKEN` is absent. |
| Winget | "Windows users can install with `winget install esqueezy.Squeezy` when the manifest is available." | `crates/squeezy-skills/external-docs/INSTALL.md:22-30`; `scripts/update_winget_manifest.sh:51-83`, `scripts/update_winget_manifest.sh:85-121`; `.github/workflows/release.yml:342-401` | First manifest PR is manual, and subsequent update job still needs `WINGET_PKGS_TOKEN`. The package targets Windows Desktop x64 and portable zip install. |
| Cargo | "Rust users can install the CLI with `cargo install squeezy --locked`; source checkouts can use `cargo install --path crates/squeezy-cli --locked`." | `README.md:45-49`; `crates/squeezy-skills/external-docs/INSTALL.md:51-71`; `Cargo.toml:24-35`; `crates/squeezy-cli/Cargo.toml:1-17` | Requires Rust 1.93.1 or newer. Pre-first-publish dry runs may be `PENDING` for crates whose workspace-internal dependencies are not yet on crates.io. |
| Local release smoke | "Release-channel scripts have a local preflight that exercises Cargo dry-run, install.sh, Homebrew formula generation, and Winget manifest generation without publishing." | `scripts/local_release_smoke.sh:1-27`, `scripts/local_release_smoke.sh:47-75`, `scripts/local_release_smoke.sh:252-281`, `scripts/local_release_smoke.sh:323-384`, `docs/internal/RELEASE_SMOKE.md:9-17`, `docs/internal/RELEASE_SMOKE.md:169-181` | The smoke does not build real distribution archives and does not run end-to-end Homebrew or Winget installs. Cargo channel is not fully network-free. |

## Release Artifact Matrix

Release workflow artifacts:

| Target | Archive | Workflow runner | Public copy posture |
| --- | --- | --- | --- |
| `x86_64-apple-darwin` | `squeezy-x86_64-apple-darwin.tar.gz` | `macos-15-intel` | Supported release artifact. Intel macOS is built and smoke-tested at release time, but not continuously test/clippy exercised in PR CI. |
| `aarch64-apple-darwin` | `squeezy-aarch64-apple-darwin.tar.gz` | `macos-15` | Supported release artifact and current ARM64 macOS CI path. |
| `x86_64-unknown-linux-musl` | `squeezy-x86_64-unknown-linux-musl.tar.gz` | `ubuntu-22.04` | Supported static Linux artifact and Homebrew Linux target. |
| `aarch64-unknown-linux-musl` | `squeezy-aarch64-unknown-linux-musl.tar.gz` | `ubuntu-22.04-arm` | Supported release artifact and install.sh target, but currently underrepresented in website copy and not Homebrew-supported. |
| `x86_64-pc-windows-msvc` | `squeezy-x86_64-pc-windows-msvc.zip` | `windows-2022` | Supported Windows release artifact and Winget package target. Unsigned today. |

Sources: `.github/workflows/release.yml:35-54`,
`.github/workflows/release.yml:124-141`, `.github/workflows/release.yml:143-190`,
`.github/workflows/release.yml:199-218`;
`crates/squeezy-skills/external-docs/PLATFORMS.md:5-50`,
`crates/squeezy-skills/external-docs/PLATFORMS.md:52-76`,
`crates/squeezy-skills/external-docs/PLATFORMS.md:78-120`.

## Packaging and Publish Posture

- The published CLI crate/package name is `squeezy`; the binary name is also
  `squeezy`. Source path remains `crates/squeezy-cli`.
  Sources: `crates/squeezy-cli/Cargo.toml:1-17`.
- Workspace package metadata sets version `0.1.0`, Rust edition 2024, Rust
  version 1.93.1, Apache-2.0 license, repo/homepage, and `publish = true`.
  Sources: `Cargo.toml:24-35`.
- Workspace members include publishable internal crates plus two explicitly
  unpublished support crates: `squeezy-eval` and `squeezy-harness`.
  Sources: `Cargo.toml:1-21`, `crates/squeezy-eval/Cargo.toml:1-13`,
  `crates/squeezy-harness/Cargo.toml:1-13`.
- Distribution artifacts use the `dist` profile with LTO, one codegen unit, and
  symbol stripping; normal Cargo release/source installs keep Cargo's practical
  default release shape with LTO off and 16 codegen units.
  Sources: `Cargo.toml:134-147`.
- Release tags must match `vMAJOR.MINOR.PATCH[-PRERELEASE]` and match
  `[workspace.package].version`.
  Sources: `.github/workflows/release.yml:57-93`,
  `scripts/update_homebrew_formula.sh:14-41`,
  `scripts/local_release_smoke.sh:144-168`.

## Platform Posture for Website Copy

Use:

- "Native release archives for macOS, Linux, and Windows."
- "macOS: Apple Silicon and Intel."
- "Linux: static musl archives for x86_64 and ARM64."
- "Windows: x86_64 portable archive and Winget path."
- "`squeezy doctor` is the first verification step after install."

Avoid:

- "Fully static binaries on every platform." Linux musl artifacts are checked
  for no dynamic interpreter/dependencies; macOS intentionally uses Apple system
  libraries, and Windows still imports OS DLLs.
- "Signed Windows releases" until code signing is implemented.
- "Homebrew works on every Linux architecture." The formula rejects non-Intel
  Linux today.
- "GPG-verified install" as an unconditional claim. The installer verifies
  SHA-256 always, while GPG is optional and depends on local key/signature
  availability.
- "Cargo install is fastest" for ordinary users. Cargo is a good path for Rust
  users, but prebuilt archives avoid a source build.

## Current Website Copy Gaps

- `squeezy-site/src/pages/install.astro` lists Linux as "x86_64 static binary"
  and `x86_64-unknown-linux-musl` only, while release automation and installer
  target detection also include `aarch64-unknown-linux-musl`.
  Sources: `squeezy-site/src/pages/install.astro:15-20`,
  `.github/workflows/release.yml:47-50`, `install.sh:90-94`.
- `squeezy-site/src/pages/docs/install.astro` has the same Linux x86_64-only
  wording.
  Sources: `squeezy-site/src/pages/docs/install.astro:22-27`.
- `squeezy-site/src/facts.ts` install rows also say Linux x86_64 only.
  Sources: `squeezy-site/src/facts.ts:293-313`.
- `crates/squeezy-skills/external-docs/INSTALL.md` says direct release archives
  publish four archives and omits Linux ARM64, but the current release workflow
  builds five. This should be reconciled before copying that exact list to the
  website.
  Sources: `crates/squeezy-skills/external-docs/INSTALL.md:73-81`,
  `.github/workflows/release.yml:35-54`.
- `crates/squeezy-skills/external-docs/INSTALL.md` also has broad top-level
  wording that says Homebrew, Cargo, and direct archives work on every platform.
  Do not copy that literally for the website: Homebrew is not the Windows path,
  and the generated formula only supports macOS plus x86_64 Linux.
  Sources: `crates/squeezy-skills/external-docs/INSTALL.md:3-6`,
  `scripts/update_homebrew_formula.sh:102-119`.
- `squeezy-site/README.md` still says public copy should be grounded in
  `docs/external/`, but this checkout stores external docs under
  `crates/squeezy-skills/external-docs/` and maps them logically at build time.
  Sources: `squeezy-site/README.md:43-48`, `crates/squeezy-skills/build.rs:4-32`.

## WIP and Caveats to Preserve

- Early development: README says the TUI scaffold is runnable, validation
  harness tasks run in CI, and graph-backed navigation tools expose compact
  evidence packets. Do not overstate general availability.
  Source: `README.md:8-24`.
- Windows code signing is not done. SmartScreen warnings are expected on first
  launch, and Azure Trusted Signing is roadmap language.
  Sources: `crates/squeezy-skills/external-docs/INSTALL.md:108-112`,
  `crates/squeezy-skills/external-docs/PLATFORMS.md:114-116`.
- Windows shell sandboxing is best-effort-limited to Job Objects, with filesystem
  and network isolation unavailable. Install/platform pages should not bury this
  if they mention safety.
  Source: `crates/squeezy-skills/external-docs/PLATFORMS.md:100-108`.
- Intel macOS is a release smoke-test target, not a continuously test/clippy
  exercised PR-CI target.
  Source: `crates/squeezy-skills/external-docs/PLATFORMS.md:14-20`.
- Release package manager updates can skip due to missing tokens.
  Sources: `.github/workflows/release.yml:296-324`,
  `.github/workflows/release.yml:352-380`.
- Local release smoke is useful but not a substitute for published-asset package
  manager installs.
  Source: `docs/internal/RELEASE_SMOKE.md:169-181`.

## Website Copy Ideas

Install page hero:

> Install the native `squeezy` binary, run `squeezy doctor`, then start it in the
> repository you want it to understand.

Platform cards:

- macOS: "Apple Silicon and Intel release archives. Use the curl installer or
  Homebrew."
- Linux: "Static musl release archives for x86_64 and ARM64. Use the curl
  installer; Homebrew currently supports x86_64 Linux."
- Windows: "x86_64 portable archive through Winget or manual zip install.
  Windows signing is still in progress."
- Source build: "Use `cargo install squeezy --locked` when you already have Rust
  1.93.1 or newer."

Release assurance block:

> Release builds are smoke-tested with `squeezy doctor`, `--version`, and
> `--help`. Linux artifacts are checked for static musl linkage, macOS artifacts
> are checked for non-system dylib dependencies, and Windows artifacts are checked
> for static CRT linkage.

Checksum/security wording:

> The installer verifies SHA-256 sidecars before installing. If a Squeezy release
> public key and `.asc` signature are available locally, it also verifies the GPG
> signature.

WIP callout:

> Squeezy is early. macOS and Linux have prebuilt installer paths, Windows has a
> Winget/manual zip path, and Windows code signing is still on the roadmap.

## Suggested Follow-Up Before Editing Site Files

1. Decide whether the public install page should expose Linux ARM64 now. The
   workflow and installer support it, but existing public/bundled docs are
   inconsistent.
2. Reconcile `crates/squeezy-skills/external-docs/INSTALL.md` with the release
   workflow's five-archive matrix if Linux ARM64 is a supported public claim.
3. Consider updating site facts/install pages in one pass after the docs
   reconcile, so the website and bundled help do not drift.
4. If claiming Homebrew/Linux support, phrase it as x86_64 Linux only unless the
   Homebrew formula grows ARM64 Linux support.
