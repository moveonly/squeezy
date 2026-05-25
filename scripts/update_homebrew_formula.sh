#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 3 ]]; then
  echo "usage: $0 <release-tag> <release-assets-dir> <homebrew-tap-dir>" >&2
  exit 2
fi

release_tag="$1"
assets_dir="$2"
tap_dir="$3"
repository="${SQUEEZY_RELEASE_REPOSITORY:-esqueezy/squeezy}"

if [[ ! "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.+-]+)?$ ]]; then
  echo "release tag must match vMAJOR.MINOR.PATCH[-PRERELEASE]: $release_tag" >&2
  exit 1
fi

# Defense in depth: when invoked from a checkout that has Cargo.toml available,
# reject mismatches between the tag and the workspace version. The release
# workflow already enforces this earlier, but the script may be run manually.
cargo_toml="${SQUEEZY_CARGO_TOML:-Cargo.toml}"
if [[ -f "$cargo_toml" ]]; then
  workspace_version="$(
    awk '/^\[workspace\.package\]/ {in_section=1; next}
         /^\[/ {in_section=0}
         in_section && /^version[[:space:]]*=/ {
           match($0, /"[^"]+"/);
           print substr($0, RSTART + 1, RLENGTH - 2);
           exit
         }' "$cargo_toml"
  )"
  if [[ -z "$workspace_version" ]]; then
    echo "could not read workspace.package.version from $cargo_toml" >&2
    exit 1
  fi
  if [[ "${release_tag#v}" != "$workspace_version" ]]; then
    echo "release tag $release_tag does not match workspace version $workspace_version ($cargo_toml)" >&2
    exit 1
  fi
fi

if [[ ! -d "$assets_dir" ]]; then
  echo "release assets dir not found: $assets_dir" >&2
  exit 1
fi

if [[ ! -d "$tap_dir" ]]; then
  echo "Homebrew tap dir not found: $tap_dir" >&2
  exit 1
fi

sha256_for() {
  local archive="$1"
  local checksum_file
  checksum_file="$(find "$assets_dir" -type f -name "${archive}.sha256" -print -quit)"
  if [[ -z "$checksum_file" ]]; then
    echo "missing checksum for archive: $archive" >&2
    return 1
  fi
  local checksum
  checksum="$(awk '{print $1}' "$checksum_file")"
  if [[ ! "$checksum" =~ ^[A-Fa-f0-9]{64}$ ]]; then
    echo "checksum for $archive is not a 64-char hex sha256: '$checksum'" >&2
    return 1
  fi
  printf '%s' "$checksum"
}

url_for() {
  local archive="$1"
  printf 'https://github.com/%s/releases/download/%s/%s' "$repository" "$release_tag" "$archive"
}

version="${release_tag#v}"
x86_macos="squeezy-x86_64-apple-darwin.tar.gz"
arm_macos="squeezy-aarch64-apple-darwin.tar.gz"
x86_linux="squeezy-x86_64-unknown-linux-musl.tar.gz"

# Resolve every URL and checksum up-front so a missing or malformed checksum
# fails the script under `set -e` instead of silently expanding to "" inside
# the heredoc below (command-substitution failures in heredocs do not
# propagate to the parent shell on their own).
arm_macos_url="$(url_for "$arm_macos")"
arm_macos_sha="$(sha256_for "$arm_macos")"
x86_macos_url="$(url_for "$x86_macos")"
x86_macos_sha="$(sha256_for "$x86_macos")"
x86_linux_url="$(url_for "$x86_linux")"
x86_linux_sha="$(sha256_for "$x86_linux")"

formula_dir="$tap_dir/Formula"
formula_file="$formula_dir/squeezy.rb"
mkdir -p "$formula_dir"

cat > "$formula_file" <<FORMULA
class Squeezy < Formula
  desc "Cost-aware coding agent TUI with local semantic code navigation"
  homepage "https://github.com/${repository}"
  version "${version}"
  license "Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "${arm_macos_url}"
      sha256 "${arm_macos_sha}"
    else
      url "${x86_macos_url}"
      sha256 "${x86_macos_sha}"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "${x86_linux_url}"
      sha256 "${x86_linux_sha}"
    else
      odie "Squeezy only publishes x86_64 Linux Homebrew archives for now"
    end
  end

  def install
    bin.install "squeezy"
  end

  test do
    assert_match "squeezy: ok", shell_output("#{bin}/squeezy --health")
  end
end
FORMULA

echo "updated $formula_file"
