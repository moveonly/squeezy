#!/usr/bin/env python3
"""Enforce an explicit allowlist for Cargo dependencies that ship build scripts.

Mirrors pnpm's ``allowedInstallScriptPackages`` guardrail: dependencies that
execute arbitrary code at build time (``build = "build.rs"``) MUST be
enumerated in ``scripts/build_script_allowlist.txt`` before they are pulled
into the workspace. This blocks the common supply-chain pattern where a
transitive dependency silently gains a build script in a new release and
starts running unaudited code on every developer's machine and on CI.

Workspace-local crates are always permitted; we own their build scripts and
``cargo`` already shows them in regular review.

Usage:
    python3 scripts/check_build_scripts.py                  # live scan
    python3 scripts/check_build_scripts.py --self-test      # fixture tests
    python3 scripts/check_build_scripts.py --metadata FILE  # offline scan

Exit codes:
    0  no disallowed build-script crates found
    1  at least one transitive dep ships a non-allowlisted build script
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_ALLOWLIST = REPO_ROOT / "scripts" / "build_script_allowlist.txt"
COMMENT_PREFIX = "#"
VERSION_SEPARATOR = "@"


@dataclass(frozen=True)
class AllowEntry:
    """A single allowlist line: ``name`` or ``name@version``."""

    name: str
    version: str | None = None

    def matches(self, name: str, version: str) -> bool:
        if self.name != name:
            return False
        if self.version is None:
            return True
        return self.version == version

    def display(self) -> str:
        return self.name if self.version is None else f"{self.name}@{self.version}"


@dataclass
class BuildScriptPkg:
    name: str
    version: str
    src_path: str

    def label(self) -> str:
        return f"{self.name} {self.version}"


@dataclass
class Report:
    violations: list[BuildScriptPkg] = field(default_factory=list)
    allowed_hits: list[BuildScriptPkg] = field(default_factory=list)
    stale_entries: list[AllowEntry] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        # Stale allowlist entries are warn-only; they do not fail the gate
        # because a dependency removal is a *good* outcome and we do not want
        # the scanner to block the PR that removes a build-script crate.
        return not self.violations


def parse_allowlist(path: Path) -> list[AllowEntry]:
    """Parse the allowlist file. Missing file is treated as an empty allowlist."""

    entries: list[AllowEntry] = []
    if not path.is_file():
        return entries
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.split(COMMENT_PREFIX, 1)[0].strip()
        if not line:
            continue
        if VERSION_SEPARATOR in line:
            name, version = line.split(VERSION_SEPARATOR, 1)
            name = name.strip()
            version = version.strip()
            entries.append(AllowEntry(name, version or None))
        else:
            entries.append(AllowEntry(line))
    return entries


def collect_build_script_packages(metadata: dict) -> list[BuildScriptPkg]:
    """Return every non-workspace package that declares a ``custom-build`` target."""

    workspace_ids = set(metadata.get("workspace_members", []))
    hits: list[BuildScriptPkg] = []
    for pkg in metadata.get("packages", []):
        if pkg.get("id") in workspace_ids:
            continue
        for target in pkg.get("targets", []):
            kinds = target.get("kind") or []
            if "custom-build" in kinds:
                hits.append(
                    BuildScriptPkg(
                        name=pkg["name"],
                        version=pkg.get("version", "?"),
                        src_path=target.get("src_path", ""),
                    )
                )
                break
    hits.sort(key=lambda h: (h.name, h.version))
    return hits


def check(metadata: dict, allowlist: list[AllowEntry]) -> Report:
    report = Report()
    hits = collect_build_script_packages(metadata)
    used: set[int] = set()
    for hit in hits:
        match_idx: int | None = None
        for idx, entry in enumerate(allowlist):
            if entry.matches(hit.name, hit.version):
                match_idx = idx
                break
        if match_idx is None:
            report.violations.append(hit)
        else:
            used.add(match_idx)
            report.allowed_hits.append(hit)
    for idx, entry in enumerate(allowlist):
        if idx not in used:
            report.stale_entries.append(entry)
    return report


def load_metadata_from_cargo() -> dict:
    """Invoke ``cargo metadata`` and return the parsed JSON output."""

    cmd = [
        "cargo",
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--manifest-path",
        str(REPO_ROOT / "Cargo.toml"),
    ]
    result = subprocess.run(cmd, capture_output=True, text=True, check=False)
    if result.returncode != 0:
        sys.stderr.write(result.stderr)
        raise SystemExit(result.returncode)
    return json.loads(result.stdout)


def format_report(report: Report, allowlist_path: Path) -> str:
    lines: list[str] = []
    if report.violations:
        lines.append("check_build_scripts: disallowed build-script crates found")
        lines.append("")
        lines.append(
            "  These transitive dependencies ship a `build.rs` and are NOT"
        )
        lines.append(f"  listed in {rel(allowlist_path)}:")
        lines.append("")
        for pkg in report.violations:
            lines.append(f"  - {pkg.name} {pkg.version}")
            if pkg.src_path:
                lines.append(f"      source: {pkg.src_path}")
        lines.append("")
        lines.append("  Audit each build script before adding it. If the script is")
        lines.append("  benign, append the package name (one per line) to")
        lines.append(f"  {rel(allowlist_path)}.")
        lines.append("")
    if report.stale_entries:
        lines.append(
            f"check_build_scripts: {len(report.stale_entries)} stale allowlist "
            "entr(y/ies) — no longer matched by any dependency (warn-only):"
        )
        for entry in report.stale_entries:
            lines.append(f"  - {entry.display()}")
        lines.append("")
    if not report.violations and not report.stale_entries:
        return (
            f"check_build_scripts: OK ({len(report.allowed_hits)} allowlisted "
            "build-script crate(s))"
        )
    if not report.violations:
        lines.append(
            f"check_build_scripts: OK ({len(report.allowed_hits)} allowlisted "
            "build-script crate(s))"
        )
    return "\n".join(lines).rstrip()


def rel(path: Path) -> str:
    try:
        return str(path.resolve().relative_to(REPO_ROOT))
    except ValueError:
        return str(path)


# ---------------------------------------------------------------------------
# Self-test fixtures.
#
# These mirror the schema that ``cargo metadata --format-version 1`` emits so
# the scanner logic is exercised end-to-end without a real Cargo project. The
# IDs and src_path values are illustrative; only the fields the scanner reads
# (``workspace_members``, ``packages[].id``, ``packages[].name``,
# ``packages[].version``, ``packages[].targets[].kind``,
# ``packages[].targets[].src_path``) need to be realistic.
# ---------------------------------------------------------------------------

_FIXTURE_WORKSPACE_AND_ALLOWED = {
    "workspace_members": ["host 0.1.0 (path+file:///fake/repo)"],
    "packages": [
        {
            "id": "host 0.1.0 (path+file:///fake/repo)",
            "name": "host",
            "version": "0.1.0",
            "targets": [
                {"kind": ["lib"], "src_path": "/fake/repo/src/lib.rs"},
                {"kind": ["custom-build"], "src_path": "/fake/repo/build.rs"},
            ],
        },
        {
            "id": "serde 1.0.0 (registry+https://example)",
            "name": "serde",
            "version": "1.0.0",
            "targets": [
                {"kind": ["lib"], "src_path": "/cache/serde/src/lib.rs"},
                {"kind": ["custom-build"], "src_path": "/cache/serde/build.rs"},
            ],
        },
    ],
}

_FIXTURE_VIOLATION = {
    "workspace_members": ["host 0.1.0 (path+file:///fake/repo)"],
    "packages": [
        {
            "id": "host 0.1.0 (path+file:///fake/repo)",
            "name": "host",
            "version": "0.1.0",
            "targets": [{"kind": ["lib"], "src_path": "/fake/repo/src/lib.rs"}],
        },
        {
            "id": "malicious 0.1.0 (registry+https://example)",
            "name": "malicious",
            "version": "0.1.0",
            "targets": [
                {"kind": ["lib"], "src_path": "/cache/malicious/src/lib.rs"},
                {"kind": ["custom-build"], "src_path": "/cache/malicious/build.rs"},
            ],
        },
    ],
}

_FIXTURE_VERSIONED = {
    "workspace_members": [],
    "packages": [
        {
            "id": "thing 1.2.3 (registry+https://example)",
            "name": "thing",
            "version": "1.2.3",
            "targets": [
                {"kind": ["custom-build"], "src_path": "/cache/thing/build.rs"}
            ],
        }
    ],
}

_FIXTURE_NO_BUILD_SCRIPTS = {
    "workspace_members": ["host 0.1.0 (path+file:///fake/repo)"],
    "packages": [
        {
            "id": "host 0.1.0 (path+file:///fake/repo)",
            "name": "host",
            "version": "0.1.0",
            "targets": [{"kind": ["lib"], "src_path": "/fake/repo/src/lib.rs"}],
        },
        {
            "id": "regex 1.10.0 (registry+https://example)",
            "name": "regex",
            "version": "1.10.0",
            "targets": [{"kind": ["lib"], "src_path": "/cache/regex/src/lib.rs"}],
        },
    ],
}


def self_test() -> int:
    """Run synthetic-fixture tests for the scanner logic."""

    failures: list[str] = []

    def expect(condition: bool, message: str) -> None:
        if not condition:
            failures.append(message)

    # 1. Workspace-internal build scripts are ignored; allowlisted transitive
    #    build scripts pass cleanly.
    report = check(_FIXTURE_WORKSPACE_AND_ALLOWED, [AllowEntry("serde")])
    expect(report.ok, f"workspace+allowed fixture should pass: {report.violations!r}")
    expect(
        all(h.name != "host" for h in report.violations + report.allowed_hits),
        "workspace member 'host' must be filtered out",
    )
    expect(
        [h.name for h in report.allowed_hits] == ["serde"],
        f"expected ['serde'] in allowed hits, got {[h.name for h in report.allowed_hits]!r}",
    )

    # 2. A non-allowlisted transitive build-script crate causes a failure.
    report = check(_FIXTURE_VIOLATION, [AllowEntry("serde")])
    expect(
        not report.ok,
        "non-allowlisted 'malicious' build script must fail the scan",
    )
    expect(
        [v.name for v in report.violations] == ["malicious"],
        f"expected ['malicious'] violation, got {[v.name for v in report.violations]!r}",
    )
    expect(
        any(e.name == "serde" for e in report.stale_entries),
        "an unused allowlist entry should be reported as stale",
    )

    # 3. Version-pinned allowlist entries match the exact version only.
    report = check(_FIXTURE_VERSIONED, [AllowEntry("thing", "1.2.3")])
    expect(report.ok, f"exact name@version match should pass: {report.violations!r}")

    report = check(_FIXTURE_VERSIONED, [AllowEntry("thing", "9.9.9")])
    expect(
        not report.ok,
        "name@version mismatch must fail even though the bare name matches",
    )

    # 4. A workspace with no transitive build scripts is trivially OK and does
    #    not require an allowlist file.
    report = check(_FIXTURE_NO_BUILD_SCRIPTS, [])
    expect(report.ok, "no build scripts → no violations")
    expect(
        not report.allowed_hits,
        f"no build scripts → no allowed hits, got {report.allowed_hits!r}",
    )

    # 5. Empty metadata is well-formed and trivially OK.
    report = check({"workspace_members": [], "packages": []}, [])
    expect(report.ok, "empty metadata should be OK")

    # 6. Allowlist parsing handles comments, blank lines, and ``name@version``.
    parsed = parse_allowlist_from_text(
        """
        # a comment
        serde
        thiserror

        # version-pinned entry
        rustix@1.1.4
        """
    )
    expect(
        [(e.name, e.version) for e in parsed]
        == [("serde", None), ("thiserror", None), ("rustix", "1.1.4")],
        f"allowlist parsing produced unexpected entries: {parsed!r}",
    )

    if failures:
        for f in failures:
            sys.stderr.write(f"check_build_scripts self-test FAIL: {f}\n")
        return 1
    print("check_build_scripts self-test: OK (6 assertions)")
    return 0


def parse_allowlist_from_text(text: str) -> list[AllowEntry]:
    """Same parser as ``parse_allowlist`` but operating on an in-memory string."""

    entries: list[AllowEntry] = []
    for raw in text.splitlines():
        line = raw.split(COMMENT_PREFIX, 1)[0].strip()
        if not line:
            continue
        if VERSION_SEPARATOR in line:
            name, version = line.split(VERSION_SEPARATOR, 1)
            entries.append(AllowEntry(name.strip(), version.strip() or None))
        else:
            entries.append(AllowEntry(line))
    return entries


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--metadata",
        type=Path,
        help=(
            "Path to a cargo metadata JSON file. When omitted, "
            "`cargo metadata --locked` is invoked against the workspace root."
        ),
    )
    parser.add_argument(
        "--allowlist",
        type=Path,
        default=DEFAULT_ALLOWLIST,
        help=f"Path to the allowlist file (default: {rel(DEFAULT_ALLOWLIST)}).",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run synthetic-fixture tests for the scanner logic and exit.",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    allowlist = parse_allowlist(args.allowlist)
    if args.metadata is not None:
        metadata = json.loads(args.metadata.read_text(encoding="utf-8"))
    else:
        metadata = load_metadata_from_cargo()

    report = check(metadata, allowlist)
    output = format_report(report, args.allowlist)
    stream = sys.stdout if report.ok else sys.stderr
    print(output, file=stream)
    return 0 if report.ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
