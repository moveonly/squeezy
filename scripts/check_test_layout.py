#!/usr/bin/env python3
"""Enforce Squeezy's Rust source and unit-test layout.

Rules:
  1. No inline ``mod tests { ... }`` blocks in crate ``src/`` directories.
  2. No file literally named ``tests.rs`` in crate ``src/`` directories.
  3. No file literally named ``mod.rs`` in crate ``src/`` directories.
  4. Every ``<module>_tests.rs`` has a sibling ``<module>.rs`` source file.
  5. Every source file with a sibling ``<module>_tests.rs`` declares it with
     both ``#[cfg(test)]`` and ``#[path = "<module>_tests.rs"]``.
"""

from __future__ import annotations

import argparse
import re
import sys
from dataclasses import dataclass, field
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
DOC_PATH = "docs/internal/TEST_LAYOUT.md"
TEST_SUFFIX = "_tests.rs"
RS_SUFFIX = ".rs"
FORBIDDEN_TESTS_FILENAME = "tests.rs"
FORBIDDEN_MOD_FILENAME = "mod.rs"

INLINE_MOD_TESTS = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+tests\s*\{",
    re.MULTILINE,
)
EXTERNAL_MOD_TESTS = re.compile(
    r"((?:^[ \t]*#\[[^\n]+\][ \t]*\n)*)[ \t]*mod[ \t]+tests[ \t]*;",
    re.MULTILINE,
)
CFG_TEST_ATTR = re.compile(r"#\[\s*cfg\s*\(\s*(?:test\s*\)|all\s*\(\s*test\b)")


@dataclass
class Report:
    inline: list[str] = field(default_factory=list)
    forbidden_tests_rs: list[str] = field(default_factory=list)
    forbidden_mod_rs: list[str] = field(default_factory=list)
    orphan_tests: list[str] = field(default_factory=list)
    malformed_decls: list[str] = field(default_factory=list)

    @property
    def total(self) -> int:
        return (
            len(self.inline)
            + len(self.forbidden_tests_rs)
            + len(self.forbidden_mod_rs)
            + len(self.orphan_tests)
            + len(self.malformed_decls)
        )


def find_src_roots(repo_root: Path) -> list[Path]:
    roots: list[Path] = []
    for cargo_toml in sorted(repo_root.rglob("Cargo.toml")):
        if "target" in cargo_toml.parts:
            continue
        # Skip agent worktrees living under .claude/worktrees/ — those are
        # detached working copies maintained by external automation and are
        # not part of the repo's enforced layout.
        if ".claude" in cargo_toml.parts:
            continue
        src = cargo_toml.parent / "src"
        if src.is_dir():
            roots.append(src)
    return roots


def check(roots: list[Path]) -> Report:
    report = Report()
    for root in roots:
        for path in sorted(root.rglob(f"*{RS_SUFFIX}")):
            if path.name == FORBIDDEN_TESTS_FILENAME:
                report.forbidden_tests_rs.append(rel(path))
            if path.name == FORBIDDEN_MOD_FILENAME:
                report.forbidden_mod_rs.append(rel(path))

            if path.name.endswith(TEST_SUFFIX):
                sibling = path.with_name(path.name[: -len(TEST_SUFFIX)] + RS_SUFFIX)
                if not sibling.is_file():
                    report.orphan_tests.append(rel(path))
                continue

            text = path.read_text(encoding="utf-8", errors="replace")
            for match in INLINE_MOD_TESTS.finditer(text):
                line = text.count("\n", 0, match.start()) + 1
                report.inline.append(f"{rel(path)}:{line}")

            test_file = path.with_name(path.stem + TEST_SUFFIX)
            if test_file.is_file():
                issue = check_external_mod_tests(text, test_file.name)
                if issue is not None:
                    report.malformed_decls.append(f"{rel(path)}: {issue}")

    return report


def check_external_mod_tests(source: str, expected_test_filename: str) -> str | None:
    matches = list(EXTERNAL_MOD_TESTS.finditer(source))
    if not matches:
        return f'no `mod tests;` declaration found for `#[path = "{expected_test_filename}"]`'
    if len(matches) > 1:
        return "multiple `mod tests;` declarations found"

    attrs = matches[0].group(1)
    if CFG_TEST_ATTR.search(attrs) is None:
        return "`mod tests;` is missing `#[cfg(test)]`"

    expected_path = re.compile(
        rf'#\[\s*path\s*=\s*"{re.escape(expected_test_filename)}"\s*\]'
    )
    if expected_path.search(attrs) is None:
        return f'`mod tests;` is missing `#[path = "{expected_test_filename}"]`'

    return None


def format_report(report: Report) -> str:
    if report.total == 0:
        return "check_test_layout: OK"

    lines = [
        "check_test_layout: Rust layout violations found",
        f"see {DOC_PATH} for the convention",
        "",
    ]
    if report.inline:
        lines.append("Rule 1: inline `mod tests {` is forbidden")
        lines.extend(f"  - {item}" for item in report.inline)
        lines.append("")
    if report.forbidden_tests_rs:
        lines.append("Rule 2: files named `tests.rs` are forbidden")
        lines.extend(f"  - {item}" for item in report.forbidden_tests_rs)
        lines.append("")
    if report.forbidden_mod_rs:
        lines.append("Rule 3: files named `mod.rs` are forbidden")
        lines.extend(f"  - {item}" for item in report.forbidden_mod_rs)
        lines.append("")
    if report.orphan_tests:
        lines.append("Rule 4: `*_tests.rs` files need a sibling source file")
        lines.extend(f"  - {item}" for item in report.orphan_tests)
        lines.append("")
    if report.malformed_decls:
        lines.append("Rule 5: malformed external test module declarations")
        lines.extend(f"  - {item}" for item in report.malformed_decls)
        lines.append("")
    lines.append(f"Total: {report.total} violation(s)")
    return "\n".join(lines)


def rel(path: Path) -> str:
    return str(path.resolve().relative_to(REPO_ROOT))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--root",
        type=Path,
        default=REPO_ROOT,
        help="Repository root to scan.",
    )
    args = parser.parse_args(argv)

    roots = find_src_roots(args.root.resolve())
    report = check(roots)
    print(format_report(report))
    return 0 if report.total == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
