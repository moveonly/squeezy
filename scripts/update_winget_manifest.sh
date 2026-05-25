#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 3 ]]; then
  echo "usage: $0 <release-tag> <release-assets-dir> <winget-pkgs-fork-dir>" >&2
  exit 2
fi

release_tag="$1"
assets_dir="$2"
fork_dir="$3"
repository="${SQUEEZY_RELEASE_REPOSITORY:-esqueezy/squeezy}"

if [[ ! "$release_tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.+-]+)?$ ]]; then
  echo "release tag must match vMAJOR.MINOR.PATCH[-PRERELEASE]: $release_tag" >&2
  exit 1
fi

if [[ ! -d "$assets_dir" ]]; then
  echo "release assets dir not found: $assets_dir" >&2
  exit 1
fi

if [[ ! -d "$fork_dir" ]]; then
  echo "winget-pkgs fork dir not found: $fork_dir" >&2
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
archive="squeezy-x86_64-pc-windows-msvc.zip"
installer_url="$(url_for "$archive")"
installer_sha="$(sha256_for "$archive")"

# winget manifests live under a publisher-letter / publisher / package
# tree. esqueezy / Squeezy → manifests/e/esqueezy/Squeezy/<version>/.
manifest_dir="$fork_dir/manifests/e/esqueezy/Squeezy/$version"
mkdir -p "$manifest_dir"

release_date="$(date -u +%Y-%m-%d)"

cat > "$manifest_dir/esqueezy.Squeezy.installer.yaml" <<INSTALLER
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.installer.1.6.0.schema.json
PackageIdentifier: esqueezy.Squeezy
PackageVersion: ${version}
Platform:
  - Windows.Desktop
MinimumOSVersion: 10.0.17763.0
InstallerType: zip
NestedInstallerType: portable
NestedInstallerFiles:
  - RelativeFilePath: squeezy.exe
    PortableCommandAlias: squeezy
Scope: user
Installers:
  - Architecture: x64
    InstallerUrl: ${installer_url}
    InstallerSha256: ${installer_sha}
ManifestType: installer
ManifestVersion: 1.6.0
ReleaseDate: ${release_date}
INSTALLER

cat > "$manifest_dir/esqueezy.Squeezy.locale.en-US.yaml" <<LOCALE
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.defaultLocale.1.6.0.schema.json
PackageIdentifier: esqueezy.Squeezy
PackageVersion: ${version}
PackageLocale: en-US
Publisher: esqueezy
PublisherUrl: https://github.com/esqueezy
PublisherSupportUrl: https://github.com/${repository}/issues
PackageName: Squeezy
PackageUrl: https://github.com/${repository}
License: Apache-2.0
LicenseUrl: https://github.com/${repository}/blob/main/LICENSE
ShortDescription: Cost-aware coding agent TUI with local semantic code navigation.
Description: |-
  Squeezy is a coding agent that treats cost, speed, and code understanding as first-class
  citizens. It parses repositories into a persistent local semantic graph and queries that
  graph through structured tools that return compact evidence packets rather than raw file
  dumps.
Tags:
  - cli
  - coding-agent
  - llm
  - rust
  - semantic-search
  - tui
ManifestType: defaultLocale
ManifestVersion: 1.6.0
LOCALE

cat > "$manifest_dir/esqueezy.Squeezy.yaml" <<VERSION
# yaml-language-server: \$schema=https://aka.ms/winget-manifest.version.1.6.0.schema.json
PackageIdentifier: esqueezy.Squeezy
PackageVersion: ${version}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
VERSION

echo "updated $manifest_dir"
