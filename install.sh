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
#   SQUEEZY_INSTALL_CHECKSUM_BASE_URL
#                         Base URL the matching .sha256 sidecar is downloaded
#                         from. When set, the checksum is fetched from
#                         ${SQUEEZY_INSTALL_CHECKSUM_BASE_URL}/${tag}/${archive}.sha256
#                         instead of alongside the archive. This lets the
#                         checksum live on a second origin so a single-origin
#                         compromise (e.g. release-publish token theft on
#                         github.com) cannot swap both files.
#   SQUEEZY_GPG_KEY_PATH  Path to the Squeezy release public key. When this
#                         file is present and gpg is installed, the release
#                         archive's .asc signature is verified after the
#                         SHA256 check. Defaults to $HOME/.squeezy/release-key.gpg.

set -eu

REPO="esqueezy/squeezy"
DEFAULT_BASE_URL="https://github.com/${REPO}/releases/download"
LATEST_API_URL="https://api.github.com/repos/${REPO}/releases/latest"

INSTALL_DIR="${SQUEEZY_INSTALL_DIR:-$HOME/.local/bin}"
TAG="${SQUEEZY_INSTALL_TAG:-}"
BASE_URL="${SQUEEZY_INSTALL_BASE_URL:-$DEFAULT_BASE_URL}"
CHECKSUM_BASE_URL="${SQUEEZY_INSTALL_CHECKSUM_BASE_URL:-$BASE_URL}"
GPG_KEY_PATH="${SQUEEZY_GPG_KEY_PATH:-$HOME/.squeezy/release-key.gpg}"

err() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}

info() {
  printf 'install.sh: %s\n' "$*"
}

warn() {
  printf 'install.sh: %s\n' "$*" >&2
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
        arm64|aarch64) echo "aarch64-unknown-linux-musl" ;;
        *) err "unsupported Linux architecture: $arch" ;;
      esac
      ;;
    *)
      err "unsupported OS: $os (this install.sh supports macOS and Linux release archives)"
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
checksum_url="${CHECKSUM_BASE_URL}/${tag}/${archive}.sha256"
signature_url="${archive_url}.asc"

if [ "$CHECKSUM_BASE_URL" != "$BASE_URL" ]; then
  info "checksum origin: $CHECKSUM_BASE_URL (split from archive origin)"
fi

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

if ! have gpg; then
  info "(skipping GPG verify -- gpg not installed)"
elif [ ! -f "$GPG_KEY_PATH" ]; then
  info "(skipping GPG verify -- no public key found at $GPG_KEY_PATH)"
elif fetch "$signature_url" "$tmpdir/$archive.asc" 2>/dev/null; then
  gpg_home="$tmpdir/gnupg"
  mkdir -p "$gpg_home"
  chmod 700 "$gpg_home"
  if gpg --homedir "$gpg_home" --batch --quiet --import "$GPG_KEY_PATH" \
      && gpg --homedir "$gpg_home" --batch --verify \
           "$tmpdir/$archive.asc" "$tmpdir/$archive" >/dev/null 2>&1; then
    info "signature OK"
  else
    err "GPG signature verification failed for $archive"
  fi
else
  info "(skipping GPG verify -- no .asc signature published for $tag)"
fi

tar -xzf "$tmpdir/$archive" -C "$tmpdir"
if [ ! -f "$tmpdir/squeezy" ]; then
  err "archive did not contain a squeezy binary"
fi

mkdir -p "$INSTALL_DIR"
mv "$tmpdir/squeezy" "$INSTALL_DIR/squeezy"
chmod +x "$INSTALL_DIR/squeezy"
info "installed $INSTALL_DIR/squeezy"

if ! "$INSTALL_DIR/squeezy" --version >/dev/null 2>&1; then
  err "installed binary at $INSTALL_DIR/squeezy did not run ($INSTALL_DIR/squeezy --version failed)"
fi

case ":${PATH:-}:" in
  *":$INSTALL_DIR:"*)
    info "installed and ready -- run 'squeezy --help' to get started"
    ;;
  *)
    printf '\n' >&2
    warn "squeezy is installed at $INSTALL_DIR but not on your PATH yet. Add it with:"
    printf '\n  export PATH="%s:$PATH"\n\n' "$INSTALL_DIR" >&2
    warn "then run 'squeezy --help' to get started"
    ;;
esac
