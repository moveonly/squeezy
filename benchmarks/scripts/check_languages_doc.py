#!/usr/bin/env python3
"""Check docs/external/LANGUAGES.md against the benchmark language/oracle registries."""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path


def run_lines(command: list[str]) -> list[str]:
    output = subprocess.check_output(command, text=True)
    return [line.strip() for line in output.splitlines() if line.strip()]


def parse_languages(lines: list[str]) -> dict[str, dict[str, str]]:
    parsed = {}
    for line in lines:
        family, *fields = line.split("\t")
        values = {}
        for field in fields:
            key, value = field.split("=", 1)
            values[key] = value
        parsed[family] = values
    return parsed


def parse_oracles(lines: list[str]) -> dict[str, dict[str, str]]:
    parsed = {}
    for line in lines:
        oracle, *fields = line.split("\t")
        values = {}
        for field in fields:
            key, value = field.split("=", 1)
            values[key] = value
        parsed[values["family"]] = {"oracle": oracle, **values}
    return parsed


def matrix_rows(markdown: str) -> dict[str, list[str]]:
    rows = {}
    for line in markdown.splitlines():
        if not line.startswith("| `"):
            continue
        cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
        if not cells:
            continue
        family = cells[0].strip("`")
        rows[family] = cells
    return rows


def normalized_cell(cell: str) -> str:
    return cell.replace("`", "")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bench", default="target/debug/squeezy-graph-bench")
    parser.add_argument("--doc", default="docs/external/LANGUAGES.md")
    args = parser.parse_args()

    bench = Path(args.bench)
    if not bench.exists():
        subprocess.check_call(
            [
                "cargo",
                "build",
                "--manifest-path",
                "benchmarks/squeezy-graph-bench/Cargo.toml",
            ]
        )
        bench = Path("benchmarks/squeezy-graph-bench/target/debug/squeezy-graph-bench")

    languages = parse_languages(run_lines([str(bench), "--list-languages"]))
    oracles = parse_oracles(run_lines([str(bench), "--list-oracles"]))
    rows = matrix_rows(Path(args.doc).read_text(encoding="utf-8"))

    errors = []
    for family, language in languages.items():
        row = rows.get(family)
        if row is None:
            errors.append(f"missing docs/external/LANGUAGES.md row for {family}")
            continue
        kinds = language["kinds"]
        extensions = language["extensions"]
        oracle = oracles.get(family, {}).get("oracle")
        mixed = oracles.get(family, {}).get("mixed")
        row_kinds = normalized_cell(row[1])
        row_extensions = normalized_cell(row[2])
        if kinds not in row_kinds and row_kinds not in kinds:
            errors.append(f"{family}: documented kinds do not include {kinds!r}")
        if extensions not in row_extensions:
            errors.append(f"{family}: documented extensions do not include {extensions!r}")
        if oracle and oracle not in row[4]:
            errors.append(f"{family}: documented oracle does not include {oracle!r}")
        if mixed and ("yes" if mixed == "true" else "no") != row[5]:
            errors.append(f"{family}: documented mixed workload should be {mixed}")

    extra_rows = set(rows) - set(languages)
    for family in sorted(extra_rows):
        errors.append(f"docs/external/LANGUAGES.md row has no live language registry entry: {family}")

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
