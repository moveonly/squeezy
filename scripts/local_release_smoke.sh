#!/usr/bin/env bash
# Local release smoke test for Squeezy.
#
# Exercises every release channel listed in .github/workflows/release.yml
# against the current commit, without actually publishing anything. Used
# as a manual pre-flight before tagging a release so problems surface
# locally instead of mid-publish.
#
# Channels:
#   cargo     `cargo publish --dry-run --no-verify` for every publishable
#             workspace member.
#   installsh `install.sh` against a local file:// release mirror staged
#             with a stub binary.
#   homebrew  `scripts/update_homebrew_formula.sh` against staged release
#             assets, then formula field and ruby-syntax checks.
#   winget    `scripts/update_winget_manifest.sh` against staged release
#             assets, then YAML structural checks.
#
# Each channel runs in its own subdirectory under a single $TMPDIR root
# and writes its full stdout+stderr to `logs/<channel>.log`. A summary
# table reports pass/fail per channel. The script exits non-zero when
# any requested channel fails; on failure the tmpdir is preserved for
# inspection.
#
# This script NEVER pushes, publishes, or mutates state outside its
# temporary working tree. See docs/internal/RELEASE_SMOKE.md.
#
# Usage:
#   scripts/local_release_smoke.sh
#   scripts/local_release_smoke.sh --tag v1.2.3
#   scripts/local_release_smoke.sh --only cargo,installsh
#   scripts/local_release_smoke.sh --skip winget
#   scripts/local_release_smoke.sh --keep-tmpdir
#   scripts/local_release_smoke.sh --verbose

# Every smoke_<channel>, helper, and cleanup function below is dispatched
# indirectly (`"$fn"` inside run_channel, the case branches in the main
# loop, or the EXIT trap), which shellcheck cannot see.
# shellcheck disable=SC2329

set -euo pipefail
shopt -s nullglob

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$script_dir/.." && pwd)"

all_channels=(cargo installsh homebrew winget)
only=""
skip=""
tag_override=""
keep_tmpdir=0
verbose=0

usage() {
  cat <<'USAGE'
Usage: scripts/local_release_smoke.sh [OPTIONS]

Exercise every Squeezy release channel against the current commit in
isolated temporary directories. Nothing is published.

Options:
  --tag <vX.Y.Z>      Release tag to test against. Defaults to
                      v<workspace.package.version> from Cargo.toml.
  --only <c1,c2>      Only run the listed channels. One of: cargo,
                      installsh, homebrew, winget.
  --skip <c1,c2>      Skip the listed channels.
  --keep-tmpdir       Keep the tmp working tree even on success.
  --verbose           Stream per-channel logs as channels run.
  -h, --help          Show this help and exit.

Exit codes:
  0  Every requested channel passed.
  1  At least one requested channel failed.
  2  Misuse (bad args).
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tag)
      [[ $# -ge 2 ]] || { echo "--tag requires an argument" >&2; exit 2; }
      tag_override="$2"; shift 2 ;;
    --only)
      [[ $# -ge 2 ]] || { echo "--only requires an argument" >&2; exit 2; }
      only="$2"; shift 2 ;;
    --skip)
      [[ $# -ge 2 ]] || { echo "--skip requires an argument" >&2; exit 2; }
      skip="$2"; shift 2 ;;
    --keep-tmpdir) keep_tmpdir=1; shift ;;
    --verbose) verbose=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

validate_channels() {
  local raw="$1"
  local label="$2"
  local IFS=','
  local c
  for c in $raw; do
    [[ -z "$c" ]] && continue
    local ok=0
    local known
    for known in "${all_channels[@]}"; do
      if [[ "$c" == "$known" ]]; then ok=1; break; fi
    done
    if (( ok == 0 )); then
      echo "$label: unknown channel '$c'; known channels: ${all_channels[*]}" >&2
      exit 2
    fi
  done
}
[[ -n "$only" ]] && validate_channels "$only" "--only"
[[ -n "$skip" ]] && validate_channels "$skip" "--skip"

list_contains() {
  local raw="$1"
  local needle="$2"
  local IFS=','
  local c
  for c in $raw; do
    [[ "$c" == "$needle" ]] && return 0
  done
  return 1
}

is_enabled() {
  local channel="$1"
  if [[ -n "$only" ]]; then
    list_contains "$only" "$channel"
    return $?
  fi
  if [[ -n "$skip" ]] && list_contains "$skip" "$channel"; then
    return 1
  fi
  return 0
}

read_workspace_version() {
  awk '/^\[workspace\.package\]/ {in_section=1; next}
       /^\[/ {in_section=0}
       in_section && /^version[[:space:]]*=/ {
         match($0, /"[^"]+"/)
         print substr($0, RSTART + 1, RLENGTH - 2)
         exit
       }' "$repo_root/Cargo.toml"
}

if [[ -n "$tag_override" ]]; then
  tag="$tag_override"
else
  ws_version="$(read_workspace_version)"
  if [[ -z "$ws_version" ]]; then
    echo "could not read workspace.package.version from $repo_root/Cargo.toml" >&2
    exit 1
  fi
  tag="v$ws_version"
fi

if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.+-]+)?$ ]]; then
  echo "tag must match vMAJOR.MINOR.PATCH[-PRERELEASE]: $tag" >&2
  exit 2
fi

_tmpdir_base="${TMPDIR:-/tmp}"
_tmpdir_base="${_tmpdir_base%/}"
tmpdir="$(mktemp -d "$_tmpdir_base/squeezy-release-smoke.XXXXXX")"
unset _tmpdir_base
logs_dir="$tmpdir/logs"
mkdir -p "$logs_dir"

had_failures=0
cleanup_tmpdir() {
  if (( keep_tmpdir == 1 )) || (( had_failures == 1 )); then
    echo "tmpdir preserved at: $tmpdir" >&2
  else
    rm -rf "$tmpdir"
  fi
}
trap cleanup_tmpdir EXIT

channel_names=()
channel_status=()
channel_elapsed=()

record_result() {
  channel_names+=("$1")
  channel_status+=("$2")
  channel_elapsed+=("$3")
}

now_seconds() { date +%s; }

run_channel() {
  local channel="$1"
  local fn="$2"
  local log="$logs_dir/$channel.log"
  printf '[%s] starting...\n' "$channel"
  local start; start="$(now_seconds)"
  local rc=0
  if "$fn" >"$log" 2>&1; then rc=0; else rc=$?; fi
  local end; end="$(now_seconds)"
  local elapsed=$((end - start))
  if (( verbose == 1 )); then
    sed 's/^/  | /' "$log" || true
  fi
  if (( rc == 0 )); then
    printf '[%s] PASS (%ss) -> %s\n' "$channel" "$elapsed" "$log"
    record_result "$channel" "PASS" "$elapsed"
  else
    printf '[%s] FAIL (%ss, exit %s) -> %s\n' "$channel" "$elapsed" "$rc" "$log"
    record_result "$channel" "FAIL" "$elapsed"
  fi
}

make_stub_binary() {
  local out="$1"
  cat >"$out" <<'STUB'
#!/bin/sh
# Stub `squeezy` used by scripts/local_release_smoke.sh. Real release
# binaries are built by cargo + the release workflow; this stub only
# exists so that install.sh has something to extract and so that the
# Homebrew/winget generators have a sha256-able file to point at.
case "${1:-}" in
  --version) printf 'squeezy 0.0.0-smoke-stub\n' ;;
  doctor) printf 'squeezy stub doctor: ok (smoke stub, no checks performed)\n' ;;
  --help|-h|help) printf 'squeezy stub (release smoke). Supported: --version, doctor.\n' ;;
  *) printf 'squeezy stub binary (release smoke test)\n' ;;
esac
STUB
  chmod +x "$out"
}

sha256_file_into() {
  local archive_dir="$1"
  local archive="$2"
  if command -v shasum >/dev/null 2>&1; then
    (cd "$archive_dir" && shasum -a 256 "$archive" > "$archive.sha256")
  elif command -v sha256sum >/dev/null 2>&1; then
    (cd "$archive_dir" && sha256sum "$archive" > "$archive.sha256")
  else
    echo "no shasum or sha256sum on PATH" >&2
    return 1
  fi
}

stage_release_assets() {
  local out_dir="$1"
  local stub_binary="$tmpdir/stub-binary"
  [[ -x "$stub_binary" ]] || make_stub_binary "$stub_binary"
  mkdir -p "$out_dir"

  local target archive stage
  for target in \
    x86_64-apple-darwin \
    aarch64-apple-darwin \
    x86_64-unknown-linux-musl \
    aarch64-unknown-linux-musl
  do
    archive="squeezy-${target}.tar.gz"
    stage="$out_dir/.stage-$target"
    mkdir -p "$stage"
    cp "$stub_binary" "$stage/squeezy"
    chmod +x "$stage/squeezy"
    tar -C "$stage" -czf "$out_dir/$archive" squeezy
    rm -rf "$stage"
    sha256_file_into "$out_dir" "$archive"
  done

  # Windows zip — the generators only read the .sha256 sidecar, so we
  # stage a placeholder file rather than building a real zip on a
  # non-Windows host.
  local zip_archive="squeezy-x86_64-pc-windows-msvc.zip"
  cp "$stub_binary" "$out_dir/$zip_archive"
  sha256_file_into "$out_dir" "$zip_archive"
}

discover_publishable_crates() {
  local cargo_toml crate_name
  for cargo_toml in "$repo_root"/crates/*/Cargo.toml; do
    crate_name="$(
      awk '/^\[package\]/ {in_pkg=1; next}
           /^\[/ {in_pkg=0}
           in_pkg && /^name[[:space:]]*=/ {
             match($0, /"[^"]+"/)
             print substr($0, RSTART + 1, RLENGTH - 2)
             exit
           }' "$cargo_toml"
    )"
    [[ -n "$crate_name" ]] || continue
    if awk '/^\[package\]/ {in_pkg=1; next}
            /^\[/ {in_pkg=0}
            in_pkg && /^publish[[:space:]]*=[[:space:]]*false/ { found=1 }
            END { exit (found ? 0 : 1) }' "$cargo_toml"; then
      continue
    fi
    printf '%s\n' "$crate_name"
  done
}

# A `cargo publish --dry-run` failure that consists solely of a
# `no matching package named 'squeezy-…' found` error means the workspace
# member would publish successfully once its workspace-internal
# dependencies are themselves on crates.io. Pre-first-publish this is
# the expected state for every crate that depends on another workspace
# member, so we tolerate it as "PENDING" rather than failing the smoke
# (see docs/internal/RELEASE_SMOKE.md). Anything else is a real failure.
is_only_workspace_internal_dep_error() {
  local log="$1"
  if ! grep -qE "no matching package named [\`'\"]squeezy[A-Za-z0-9_-]*[\`'\"] found" "$log"; then
    return 1
  fi
  local extra
  extra="$(grep '^error:' "$log" | grep -v -F 'failed to prepare local package for uploading' || true)"
  [[ -z "$extra" ]]
}

smoke_cargo() {
  if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not found on PATH" >&2
    return 1
  fi

  local target_dir="$tmpdir/cargo-target"
  mkdir -p "$target_dir"
  export CARGO_TARGET_DIR="$target_dir"

  local crates
  crates="$(discover_publishable_crates)"
  if [[ -z "$crates" ]]; then
    echo "no publishable workspace members detected under $repo_root/crates" >&2
    return 1
  fi

  echo "publishable workspace members:"
  while IFS= read -r crate; do
    [[ -z "$crate" ]] && continue
    printf '  %s\n' "$crate"
  done <<<"$crates"
  echo
  echo "(workspace-internal dependencies that are not yet on crates.io"
  echo " are treated as PENDING rather than failures — see"
  echo " docs/internal/RELEASE_SMOKE.md.)"
  echo

  local ok=0 pending=0 failed=0 crate
  local failed_crates="" pending_crates=""
  local per_crate_log
  while IFS= read -r crate; do
    [[ -z "$crate" ]] && continue
    per_crate_log="$target_dir/.smoke-$crate.log"
    echo "=== cargo publish --dry-run --no-verify --allow-dirty -p $crate ==="
    if (cd "$repo_root" && cargo publish --dry-run --no-verify --allow-dirty -p "$crate") \
        >"$per_crate_log" 2>&1; then
      sed 's/^/    /' "$per_crate_log"
      echo "[cargo] $crate: PASS"
      ok=$((ok + 1))
    else
      sed 's/^/    /' "$per_crate_log"
      if is_only_workspace_internal_dep_error "$per_crate_log"; then
        echo "[cargo] $crate: PENDING (waiting on workspace-internal deps to land on crates.io)"
        pending=$((pending + 1))
        pending_crates="${pending_crates}${pending_crates:+, }$crate"
      else
        echo "[cargo] $crate: FAIL"
        failed=$((failed + 1))
        failed_crates="${failed_crates}${failed_crates:+, }$crate"
      fi
    fi
    echo
  done <<<"$crates"

  echo "cargo summary: $ok passed, $pending pending, $failed failed"
  [[ -n "$pending_crates" ]] && echo "  pending: $pending_crates"
  if (( failed > 0 )); then
    echo "  failed: $failed_crates"
    return 1
  fi
}

host_target_triple() {
  local os arch
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64|amd64)  echo "x86_64-apple-darwin" ;;
        *) return 1 ;;
      esac ;;
    Linux)
      case "$arch" in
        x86_64|amd64)  echo "x86_64-unknown-linux-musl" ;;
        arm64|aarch64) echo "aarch64-unknown-linux-musl" ;;
        *) return 1 ;;
      esac ;;
    *) return 1 ;;
  esac
}

smoke_installsh() {
  local channel_root="$tmpdir/installsh"
  local mirror="$channel_root/mirror/$tag"
  local install_dir="$channel_root/installdir"
  mkdir -p "$mirror" "$install_dir"

  local host_target
  if ! host_target="$(host_target_triple)"; then
    echo "install.sh smoke supports only macOS or Linux (got $(uname -sm))" >&2
    return 1
  fi
  echo "host target: $host_target"

  local stub_binary="$tmpdir/stub-binary"
  [[ -x "$stub_binary" ]] || make_stub_binary "$stub_binary"

  local stage="$channel_root/stage"
  mkdir -p "$stage"
  cp "$stub_binary" "$stage/squeezy"
  chmod +x "$stage/squeezy"
  local archive="squeezy-${host_target}.tar.gz"
  tar -C "$stage" -czf "$mirror/$archive" squeezy
  rm -rf "$stage"
  sha256_file_into "$mirror" "$archive"

  echo "mirror layout:"
  ls -la "$mirror"
  echo

  if [[ ! -f "$repo_root/install.sh" ]]; then
    echo "install.sh missing at $repo_root/install.sh" >&2
    return 1
  fi

  echo "=== running install.sh against file://$channel_root/mirror ==="
  # Point install.sh at an empty `gpg key path` so the optional gpg
  # verify step is exercised as "skipped" rather than tripping over a
  # developer's locally installed release key.
  SQUEEZY_INSTALL_DIR="$install_dir" \
  SQUEEZY_INSTALL_TAG="$tag" \
  SQUEEZY_INSTALL_BASE_URL="file://$channel_root/mirror" \
  SQUEEZY_GPG_KEY_PATH="$tmpdir/never-exists/release-key.gpg" \
    sh "$repo_root/install.sh"

  echo
  echo "=== verifying install output ==="
  if [[ ! -x "$install_dir/squeezy" ]]; then
    echo "install.sh did not place an executable squeezy at $install_dir/squeezy" >&2
    ls -la "$install_dir" >&2 || true
    return 1
  fi
  echo "binary installed at $install_dir/squeezy"
  echo "binary --version output:"
  "$install_dir/squeezy" --version
}

smoke_homebrew() {
  local channel_root="$tmpdir/homebrew"
  local assets_dir="$channel_root/assets"
  local tap_dir="$channel_root/tap"
  mkdir -p "$assets_dir" "$tap_dir"

  if [[ ! -x "$repo_root/scripts/update_homebrew_formula.sh" ]]; then
    echo "scripts/update_homebrew_formula.sh missing or not executable" >&2
    return 1
  fi

  echo "staging release assets in $assets_dir"
  stage_release_assets "$assets_dir"

  echo "running update_homebrew_formula.sh $tag $assets_dir $tap_dir"
  SQUEEZY_CARGO_TOML="$repo_root/Cargo.toml" \
    "$repo_root/scripts/update_homebrew_formula.sh" "$tag" "$assets_dir" "$tap_dir"

  local formula="$tap_dir/Formula/squeezy.rb"
  if [[ ! -s "$formula" ]]; then
    echo "formula was not written to $formula" >&2
    return 1
  fi

  echo
  echo "=== generated formula ($formula) ==="
  cat "$formula"
  echo

  local version="${tag#v}"
  local needles=(
    "class Squeezy < Formula"
    "version \"$version\""
    "on_macos"
    "on_linux"
    "sha256 "
    "bin.install \"squeezy\""
  )
  local needle
  for needle in "${needles[@]}"; do
    if ! grep -qF -- "$needle" "$formula"; then
      echo "formula missing expected snippet: $needle" >&2
      return 1
    fi
  done
  echo "formula contains every expected field"

  if command -v ruby >/dev/null 2>&1; then
    echo "running ruby -c on formula"
    ruby -c "$formula"
  else
    echo "(skipping ruby -c — ruby not on PATH)"
  fi
}

smoke_winget() {
  local channel_root="$tmpdir/winget"
  local assets_dir="$channel_root/assets"
  local fork_dir="$channel_root/winget-fork"
  mkdir -p "$assets_dir" "$fork_dir"

  if [[ ! -x "$repo_root/scripts/update_winget_manifest.sh" ]]; then
    echo "scripts/update_winget_manifest.sh missing or not executable" >&2
    return 1
  fi

  echo "staging release assets in $assets_dir"
  stage_release_assets "$assets_dir"

  echo "running update_winget_manifest.sh $tag $assets_dir $fork_dir"
  "$repo_root/scripts/update_winget_manifest.sh" "$tag" "$assets_dir" "$fork_dir"

  local version="${tag#v}"
  local manifest_dir="$fork_dir/manifests/e/esqueezy/Squeezy/$version"
  if [[ ! -d "$manifest_dir" ]]; then
    echo "manifest dir was not created: $manifest_dir" >&2
    return 1
  fi

  local manifest
  for manifest in \
    esqueezy.Squeezy.installer.yaml \
    esqueezy.Squeezy.locale.en-US.yaml \
    esqueezy.Squeezy.yaml
  do
    if [[ ! -s "$manifest_dir/$manifest" ]]; then
      echo "winget manifest missing: $manifest_dir/$manifest" >&2
      return 1
    fi
    echo "found $manifest_dir/$manifest"
  done

  echo
  echo "=== installer manifest ==="
  cat "$manifest_dir/esqueezy.Squeezy.installer.yaml"
  echo

  if grep -qF "PackageVersion: $version" "$manifest_dir/esqueezy.Squeezy.installer.yaml" \
     && grep -qF "PackageVersion: $version" "$manifest_dir/esqueezy.Squeezy.yaml" \
     && grep -qF "PackageVersion: $version" "$manifest_dir/esqueezy.Squeezy.locale.en-US.yaml"; then
    echo "every manifest references PackageVersion $version"
  else
    echo "one or more manifests is missing PackageVersion $version" >&2
    return 1
  fi

  if command -v python3 >/dev/null 2>&1; then
    echo "validating YAML structure with python3"
    python3 - "$manifest_dir" <<'PY'
import pathlib
import sys

base = pathlib.Path(sys.argv[1])
try:
    import yaml  # PyYAML — optional.
except Exception as exc:
    print(f"(PyYAML not available, skipping safe_load: {exc})")
    sys.exit(0)
for path in sorted(base.glob("*.yaml")):
    with open(path, "r", encoding="utf-8") as fh:
        yaml.safe_load(fh)
    print(f"yaml ok: {path.name}")
PY
  else
    echo "(skipping yaml structural check — python3 not on PATH)"
  fi
}

echo "=== Squeezy local release smoke ==="
echo "repo root: $repo_root"
echo "tag:       $tag"
echo "tmpdir:    $tmpdir"
echo

for channel in "${all_channels[@]}"; do
  if is_enabled "$channel"; then
    case "$channel" in
      cargo)     run_channel cargo     smoke_cargo ;;
      installsh) run_channel installsh smoke_installsh ;;
      homebrew)  run_channel homebrew  smoke_homebrew ;;
      winget)    run_channel winget    smoke_winget ;;
    esac
  else
    printf '[%s] SKIP\n' "$channel"
    record_result "$channel" "SKIP" "0"
  fi
done

echo
echo "Summary"
echo "  channel    status  elapsed  log"
fails=0
ran=0
for i in "${!channel_names[@]}"; do
  status="${channel_status[$i]}"
  name="${channel_names[$i]}"
  elapsed="${channel_elapsed[$i]}"
  case "$status" in
    SKIP)
      printf '  %-10s %-6s %4ss\n' "$name" "$status" "$elapsed"
      ;;
    *)
      printf '  %-10s %-6s %4ss   %s\n' "$name" "$status" "$elapsed" "$logs_dir/$name.log"
      ;;
  esac
  case "$status" in
    FAIL) fails=$((fails + 1)); ran=$((ran + 1)) ;;
    PASS) ran=$((ran + 1)) ;;
  esac
done

echo
if (( fails == 0 )); then
  echo "All $ran channel(s) passed."
  exit 0
else
  had_failures=1
  echo "$fails of $ran channel(s) FAILED."
  echo
  echo "Last 30 lines of each failed log:"
  for i in "${!channel_names[@]}"; do
    if [[ "${channel_status[$i]}" == "FAIL" ]]; then
      name="${channel_names[$i]}"
      log="$logs_dir/$name.log"
      echo
      echo "--- $name ($log) ---"
      tail -n 30 "$log" 2>/dev/null || echo "(log not readable)"
    fi
  done
  exit 1
fi
