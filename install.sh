#!/bin/sh
# Squeezy one-line installer.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/esqueezy/squeezy/main/install.sh | sh
#
# Honored environment variables:
#   SQUEEZY_INSTALL_DIR   Directory the squeezy binary is installed into
#                         (default: $HOME/.local/bin).
#   SQUEEZY_INSTALL_TAG   Release tag to install (default: latest release).
#   SQUEEZY_INSTALL_BASE_URL
#                         Base URL releases are downloaded from. Defaults to
#                         https://github.com/esqueezy/squeezy/releases/download.
#                         Mainly useful for CI smoke tests against a local
#                         file:// mirror.

set -eu

REPO="esqueezy/squeezy"
DEFAULT_BASE_URL="https://github.com/${REPO}/releases/download"
LATEST_API_URL="https://api.github.com/repos/${REPO}/releases/latest"

INSTALL_DIR="${SQUEEZY_INSTALL_DIR:-$HOME/.local/bin}"
TAG="${SQUEEZY_INSTALL_TAG:-}"
BASE_URL="${SQUEEZY_INSTALL_BASE_URL:-$DEFAULT_BASE_URL}"

err() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

info() {
  printf 'install.sh: %s\n' "$*"
}

have() {
  command -v "$1" >/dev/null 2>&1
}

require() {
  have "$1" || err "missing required tool: $1"
}

require uname
require tar
require mkdir
require chmod
if have curl; then
  fetch() { curl -fsSL "$1" -o "$2"; }
  fetch_stdout() { curl -fsSL "$1"; }
elif have wget; then
  fetch() { wget -q -O "$2" "$1"; }
  fetch_stdout() { wget -q -O - "$1"; }
else
  err "need curl or wget on PATH"
fi
if have shasum; then
  sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
elif have sha256sum; then
  sha256_of() { sha256sum "$1" | awk '{print $1}'; }
else
  err "need shasum or sha256sum on PATH"
fi

detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin)
      case "$arch" in
        arm64|aarch64) echo "aarch64-apple-darwin" ;;
        x86_64|amd64) echo "x86_64-apple-darwin" ;;
        *) err "unsupported macOS architecture: $arch" ;;
      esac
      ;;
    Linux)
      case "$arch" in
        x86_64|amd64) echo "x86_64-unknown-linux-musl" ;;
        *) err "unsupported Linux architecture: $arch (only x86_64 is published)" ;;
      esac
      ;;
    *)
      err "unsupported OS: $os (squeezy publishes macOS and Linux only)"
      ;;
  esac
}

resolve_tag() {
  if [ -n "$TAG" ]; then
    printf '%s' "$TAG"
    return
  fi
  # Latest-release endpoint returns JSON with "tag_name": "v0.1.2". Extract
  # without depending on jq.
  body="$(fetch_stdout "$LATEST_API_URL")" || err "could not query $LATEST_API_URL"
  echo "$body" | tr ',' '\n' \
    | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1
}

target="$(detect_target)"
tag="$(resolve_tag)"
[ -n "$tag" ] || err "could not resolve a release tag (set SQUEEZY_INSTALL_TAG to override)"

archive="squeezy-${target}.tar.gz"
archive_url="${BASE_URL}/${tag}/${archive}"
checksum_url="${archive_url}.sha256"

tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t squeezy-install)"
cleanup() { rm -rf "$tmpdir"; }
trap cleanup EXIT INT HUP TERM

info "downloading $archive (${tag})"
fetch "$archive_url" "$tmpdir/$archive" || err "download failed: $archive_url"
fetch "$checksum_url" "$tmpdir/$archive.sha256" || err "download failed: $checksum_url"

expected="$(awk '{print $1}' "$tmpdir/$archive.sha256")"
case "$expected" in
  [A-Fa-f0-9]*) ;;
  *) err "checksum file is not a hex digest: $expected" ;;
esac
actual="$(sha256_of "$tmpdir/$archive")"
if [ "$expected" != "$actual" ]; then
  err "checksum mismatch for $archive (expected $expected, got $actual)"
fi
info "checksum ok"

tar -xzf "$tmpdir/$archive" -C "$tmpdir"
if [ ! -f "$tmpdir/squeezy" ]; then
  err "archive did not contain a squeezy binary"
fi

mkdir -p "$INSTALL_DIR"
mv "$tmpdir/squeezy" "$INSTALL_DIR/squeezy"
chmod +x "$INSTALL_DIR/squeezy"
info "installed $INSTALL_DIR/squeezy"

case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    printf '\n'
    info "$INSTALL_DIR is not on your PATH yet. Add it with:"
    printf '\n  export PATH="%s:$PATH"\n\n' "$INSTALL_DIR"
    ;;
esac

info "run 'squeezy --help' to get started"
