#!/usr/bin/env python3
"""Write GitHub-step summaries for semantic graph benchmark reports."""

from __future__ import annotations

import argparse
import glob
import json
from pathlib import Path
from typing import Any


def load_reports(pattern: str) -> list[tuple[str, dict[str, Any]]]:
    reports: list[tuple[str, dict[str, Any]]] = []
    for path in sorted(glob.glob(pattern, recursive=True)):
        with open(path, encoding="utf-8") as handle:
            reports.append((path, json.load(handle)))
    return reports


def metric_or_status(obj: dict[str, Any], ms_key: str, status_key: str) -> Any:
    value = obj.get(ms_key)
    if value is not None:
        return value
    return obj.get(status_key)


def write_query_table(out: list[str], report: dict[str, Any]) -> None:
    queries = report.get("queries") or []
    if not queries:
        return
    out.append("")
    out.append("| Query | Actual | Missing | Extras |")
    out.append("|---|---:|---:|---:|")
    for query in queries:
        out.append(
            "| {id} | {actual} | {missing} | {extras} |".format(
                id=query["id"],
                actual=len(query.get("actual", [])),
                missing=len(query.get("missing", [])),
                extras=len(query.get("extras", [])),
            )
        )


def write_graph_summary(out: list[str], report: dict[str, Any]) -> None:
    graph = report.get("graph") or {}
    if graph:
        out.append(
            "- Graph: files={files} symbols={symbols} edges={edges} "
            "references={references} calls={calls}".format(
                files=graph.get("files"),
                symbols=graph.get("symbols"),
                edges=graph.get("edges"),
                references=graph.get("references"),
                calls=graph.get("calls"),
            )
        )


def write_mixed_common(out: list[str], mixed: dict[str, Any], compiler_label: str) -> None:
    out.append(f"- Mixed repo: {mixed.get('repo')}")
    out.append(f"- Available scenarios: {mixed.get('available_scenarios')}")
    out.append(f"- Executed scenarios: {mixed.get('executed_scenarios')}")
    if mixed.get("tools"):
        out.append(f"- Tools: {', '.join(mixed['tools'])}")
    compiler = metric_or_status(mixed, "compiler_check_ms", "compiler_check_status")
    if compiler is not None:
        out.append(f"- {compiler_label}: {compiler}")
    if mixed.get("rust_analyzer_ms") is not None or mixed.get("rust_analyzer_status") is not None:
        out.append(
            f"- rust-analyzer: {metric_or_status(mixed, 'rust_analyzer_ms', 'rust_analyzer_status')}"
        )
    out.append(f"- Squeezy total: {mixed.get('squeezy_total_ms')} ms")
    if mixed.get("query_time_ms"):
        out.append(f"- Query time by tool: {mixed.get('query_time_ms')}")
    refresh = mixed.get("refresh_probe")
    if refresh:
        out.append(f"- Refresh after edits: {refresh.get('refresh_ms')} ms")
        out.append(f"- Reparsed files: {refresh.get('reparsed_files')}")
    acc = ((mixed.get("accuracy") or {}).get("symbols")) or {}
    if acc:
        out.append(
            "- Mixed symbol accuracy: TP={tp} FP={fp} FN={fn} precision={precision} recall={recall}".format(
                tp=acc.get("true_positive"),
                fp=acc.get("false_positive"),
                fn=acc.get("false_negative"),
                precision=acc.get("precision"),
                recall=acc.get("recall"),
            )
        )


def rust_summary(reports: list[tuple[str, dict[str, Any]]]) -> str:
    reports = sorted(
        reports,
        key=lambda item: (item[0] != "target/semantic-graph-benchmark/rust-smoke.json", item[0]),
    )
    out = ["## Rust Semantic Graph Benchmark"]
    for path, report in reports:
        out.extend(["", f"### {path}"])
        out.append(f"- Validation: {report['validation_status']} in {report['validation_ms']} ms")
        out.append(f"- Squeezy graph build: {report['squeezy_build_ms']} ms")
        out.append(f"- Squeezy graph queries: {report['squeezy_query_ms']} ms")
        acc = report["accuracy"]["symbols"]
        out.append(
            "- Fixture symbol accuracy: TP={true_positive} FP={false_positive} "
            "FN={false_negative} precision={precision} recall={recall}".format(**acc)
        )
        out.append(
            "- Fixture symbol scope: comparable RA={rust_analyzer_total} "
            "raw RA={rust_analyzer_raw_total} excluded RA={rust_analyzer_excluded_by_kind} "
            "comparable Squeezy={squeezy_total} raw Squeezy={squeezy_raw_total} "
            "excluded Squeezy={squeezy_excluded_by_kind}".format(**acc)
        )
        nav = report["accuracy"]["navigation"]
        defs = nav["definitions"]
        refs = nav["references"]
        lsp = nav["rust_analyzer_lsp_ms"] or nav["rust_analyzer_lsp_status"]
        out.append(
            "- Fixture navigation accuracy: definition probes={probes}/{available_probes} "
            "TP={true_positive} FP={false_positive} FN={false_negative} "
            "Squeezy-only={squeezy_only} wrong-target={wrong_target}; "
            "reference symbols={ref_symbols}/{available_symbols} TP={ref_tp} "
            "FP={ref_fp} FN={ref_fn}; RA LSP={lsp}".format(
                probes=defs["probes"],
                available_probes=defs["available_probes"],
                true_positive=defs["true_positive"],
                false_positive=defs["false_positive"],
                false_negative=defs["false_negative"],
                squeezy_only=defs["squeezy_only"],
                wrong_target=defs["wrong_target"],
                ref_symbols=refs["symbols_sampled"],
                available_symbols=refs["available_symbols"],
                ref_tp=refs["true_positive"],
                ref_fp=refs["false_positive"],
                ref_fn=refs["false_negative"],
                lsp=lsp,
            )
        )
        if report.get("mixed_workload"):
            write_mixed_common(out, report["mixed_workload"], "cargo check")
        write_query_table(out, report)
    return "\n".join(out) + "\n"


def c_family_summary(reports: list[tuple[str, dict[str, Any]]]) -> str:
    out = ["## C/C++ Semantic Graph Benchmark"]
    for path, report in reports:
        out.extend(["", f"### {path}"])
        out.append(f"- Language: {report['language']}")
        out.append(f"- Fixture: {report['fixture']}")
        out.append(f"- Validation: {report['validation_status']} in {report['validation_ms']} ms")
        out.append(f"- Squeezy total: {report['squeezy_total_ms']} ms")
        write_graph_summary(out, report)
        out.append(f"- Accuracy oracle: {report['accuracy']['rust_analyzer_symbol_status']}")
        acc = report["accuracy"]["symbols"]
        out.append(
            "- Symbol TP/FP/FN: {true_positive}/{false_positive}/{false_negative} "
            "precision={precision} recall={recall}".format(**acc)
        )
        write_query_table(out, report)
        if report.get("mixed_workload"):
            out.append("")
            write_mixed_common(out, report["mixed_workload"], "Compiler validation")
            mixed_acc = report["mixed_workload"].get("accuracy", {}).get("symbols", {})
            if mixed_acc.get("squeezy_excluded_by_kind") is not None:
                out.append(f"- Oracle exclusions: {mixed_acc['squeezy_excluded_by_kind']}")
    return "\n".join(out) + "\n"


def csharp_summary(reports: list[tuple[str, dict[str, Any]]]) -> str:
    out = ["## C# Semantic Graph Benchmark"]
    for path, report in reports:
        out.extend(["", f"### {path}"])
        out.append(f"- Validation: {report['validation_status']} in {report['validation_ms']} ms")
        out.append(f"- Squeezy total: {report['squeezy_total_ms']} ms")
        write_graph_summary(out, report)
        out.append(f"- Accuracy oracle: {report['accuracy']['rust_analyzer_symbol_status']}")
        oracle = report.get("csharp_oracle")
        if oracle:
            symbols = oracle["symbols"]
            build_ms = oracle.get("oracle_build_ms")
            build = f"{build_ms} ms" if build_ms is not None else "cached"
            out.append(
                "- Roslyn oracle: tp={tp} fp={fp} fn={fn} precision={precision:.3f} "
                "recall={recall:.3f} oracle={oracle_ms} ms build={build} "
                "unparseable={unparseable}".format(
                    tp=symbols["true_positive"],
                    fp=symbols["false_positive"],
                    fn=symbols["false_negative"],
                    precision=symbols["precision"],
                    recall=symbols["recall"],
                    oracle_ms=oracle["oracle_ms"],
                    build=build,
                    unparseable=oracle["oracle_unparseable_files"],
                )
            )
            edges = oracle.get("edges")
            if edges:
                out.append(
                    "- Roslyn edge oracle: tp={tp} fp={fp} fn={fn} precision={precision:.3f} "
                    "recall={recall:.3f}".format(
                        tp=edges["true_positive"],
                        fp=edges["false_positive"],
                        fn=edges["false_negative"],
                        precision=edges["precision"],
                        recall=edges["recall"],
                    )
                )
        if report.get("mixed_workload"):
            write_mixed_common(out, report["mixed_workload"], "dotnet build")
            out.append(
                f"- Mixed oracle status: {report['mixed_workload']['accuracy']['rust_analyzer_symbol_status']}"
            )
    return "\n".join(out) + "\n"


def python_summary(reports: list[tuple[str, dict[str, Any]]]) -> str:
    out = ["## Python Semantic Graph Benchmark"]
    for path, report in reports:
        oracle = report["python_oracle"]
        acc = oracle["symbols"]
        out.extend(["", f"### {path}"])
        out.append(f"- Fixture: {report['fixture']}")
        out.append(f"- Validation: {report['validation_status']} in {report['validation_ms']} ms")
        out.append(f"- Squeezy total: {report['squeezy_total_ms']} ms")
        out.append(
            "- Python oracle symbols: TP={true_positive} FP={false_positive} "
            "FN={false_negative} precision={precision} recall={recall}".format(**acc)
        )
        if oracle.get("oracle_unparseable_files", 0):
            out.append(f"- Python oracle unparseable files: {oracle['oracle_unparseable_files']}")
        write_query_table(out, report)
    return "\n".join(out) + "\n"


def java_summary(reports: list[tuple[str, dict[str, Any]]]) -> str:
    out = ["## Java Semantic Graph Benchmark"]
    for path, report in reports:
        oracle = report["java_oracle"]
        acc = oracle["symbols"]
        nav = oracle["navigation"]
        out.extend(["", f"### {path}"])
        out.append(f"- Fixture: {report['fixture']}")
        out.append(f"- Validation: {report['validation_status']} in {report['validation_ms']} ms")
        out.append(f"- Squeezy total: {report['squeezy_total_ms']} ms")
        out.append(
            "- Java oracle symbols: TP={true_positive} FP={false_positive} "
            "FN={false_negative} precision={precision} recall={recall}".format(**acc)
        )
        out.append(
            "- Java navigation oracle: queries={query_count} TP={true_positive} "
            "FP={false_positive} FN={false_negative} precision={precision} recall={recall}".format(
                **nav
            )
        )
        write_query_table(out, report)
    return "\n".join(out) + "\n"


def go_summary(reports: list[tuple[str, dict[str, Any]]]) -> str:
    out = ["## Go Semantic Graph Benchmark"]
    for path, report in reports:
        out.extend(["", f"### {path}"])
        out.append(f"- Fixture: {report['fixture']}")
        out.append(f"- Validation: {report['validation_status']} in {report['validation_ms']} ms")
        out.append(f"- Squeezy total: {report['squeezy_total_ms']} ms")
        out.append(f"- Faster than validation: {report['faster_than_validation']}")
        phases = report.get("build_phases") or {}
        if phases:
            out.append(
                "- Build phases: crawl={crawl_ms}ms parse={parse_ms}ms "
                "declaration_graph={declaration_graph_ms}ms full_graph={full_graph_ms}ms".format(
                    **phases
                )
            )
        graph = report.get("graph") or {}
        if graph:
            out.append(
                "- Graph counts: files={files} symbols={symbols} edges={edges} "
                "references={references} calls={calls} body_hits={body_hits}".format(**graph)
            )
            out.append(
                "- Graph indexes: body_hit_trigram_indexed={body_hit_trigram_indexed} "
                "body_hit_trigram_terms={body_hit_trigram_terms} "
                "reference_index_terms={reference_index_terms}".format(**graph)
            )
        oracle = report.get("go_oracle")
        if oracle:
            acc = oracle["symbols"]
            out.append(
                "- Go oracle symbols: TP={true_positive} FP={false_positive} "
                "FN={false_negative} precision={precision} recall={recall}".format(**acc)
            )
            out.append(
                "- Go oracle scope: oracle={rust_analyzer_total} raw_oracle={rust_analyzer_raw_total} "
                "comparable_squeezy={squeezy_total} raw_squeezy={squeezy_raw_total} "
                "excluded_squeezy={squeezy_excluded_by_kind}".format(**acc)
            )
            if oracle.get("oracle_unparseable_files", 0):
                out.append(f"- Go oracle unparseable files: {oracle['oracle_unparseable_files']}")
        refresh = report.get("refresh_probe")
        if refresh:
            out.append(
                "- Refresh probe: copied={copied_source_files} edited={edited_files} "
                "reparsed={reparsed_files} refresh_ms={refresh_ms} "
                "budget_exhausted={budget_exhausted}".format(**refresh)
            )
        iterations = report.get("heuristic_iterations") or []
        if iterations:
            out.extend(["", "| Heuristic | Status | Notes |", "|---|---|---|"])
            for item in iterations:
                out.append(
                    f"| {item['name']} | {item['status']} | {' '.join(item.get('notes', []))} |"
                )
        write_query_table(out, report)
    return "\n".join(out) + "\n"


def js_ts_summary(reports: list[tuple[str, dict[str, Any]]]) -> str:
    out = ["## JS/TS Semantic Graph Benchmark"]
    for path, report in reports:
        oracle = report.get("js_ts_oracle") or {}
        acc = oracle.get("symbols", {})
        out.extend(["", f"### {path}"])
        out.append(f"- Fixture: {report['fixture']}")
        out.append(f"- Validation: {report['validation_status']} in {report['validation_ms']} ms")
        out.append(f"- Squeezy total: {report['squeezy_total_ms']} ms")
        if oracle:
            out.append(f"- Oracle status: {oracle.get('status', 'n/a')}")
            out.append(
                "- JS/TS oracle symbols: TP={tp} FP={fp} FN={fn} "
                "precision={precision} recall={recall}".format(
                    tp=acc.get("true_positive", 0),
                    fp=acc.get("false_positive", 0),
                    fn=acc.get("false_negative", 0),
                    precision=acc.get("precision", 0),
                    recall=acc.get("recall", 0),
                )
            )
            out.append(
                "- Symbol scope: oracle={oracle_total} squeezy={squeezy_total} "
                "raw_squeezy={raw_squeezy} excluded_squeezy={excluded}".format(
                    oracle_total=acc.get("rust_analyzer_total", 0),
                    squeezy_total=acc.get("squeezy_total", 0),
                    raw_squeezy=acc.get("squeezy_raw_total", 0),
                    excluded=acc.get("squeezy_excluded_by_kind", {}),
                )
            )
        mixed = report.get("mixed_workload")
        if mixed:
            out.append("")
            out.append(f"- Mixed workload repo: {mixed['repo']}")
            out.append(
                "- Scenarios: {executed}/{available} requested={requested}".format(
                    executed=mixed["executed_scenarios"],
                    available=mixed["available_scenarios"],
                    requested=mixed["requested_scenarios"],
                )
            )
            out.append(f"- Squeezy build+query: {mixed['squeezy_total_ms']} ms")
            if mixed.get("compiler_check_ms") is not None:
                out.append(
                    f"- TS oracle: {mixed['compiler_check_ms']} ms ({mixed['compiler_check_status']})"
                )
            refresh = mixed.get("refresh_probe")
            if refresh:
                out.append(
                    "- Refresh: edited={edited_files} reparsed={reparsed_files} "
                    "in {refresh_ms} ms budget_exhausted={budget_exhausted}".format(**refresh)
                )
            mixed_accuracy = mixed.get("accuracy") or {}
            if mixed_accuracy.get("symbols"):
                macc = mixed_accuracy["symbols"]
                out.append(
                    "- Mixed oracle symbols: TP={tp} FP={fp} FN={fn} "
                    "precision={precision} recall={recall}".format(
                        tp=macc.get("true_positive", 0),
                        fp=macc.get("false_positive", 0),
                        fn=macc.get("false_negative", 0),
                        precision=macc.get("precision", 0),
                        recall=macc.get("recall", 0),
                    )
                )
            if mixed_accuracy.get("navigation"):
                nav = mixed_accuracy["navigation"]
                d = nav.get("definitions", {})
                r = nav.get("references", {})
                out.append(
                    "- Navigation: def_probes={dp}/{dap} def_tp={dtp} def_fp={dfp} "
                    "def_fn={dfn} ref_symbols={rs}/{ras} ref_tp={rtp} ref_fp={rfp} "
                    "ref_fn={rfn} oracle={oracle}ms".format(
                        dp=d.get("probes", 0),
                        dap=d.get("available_probes", 0),
                        dtp=d.get("true_positive", 0),
                        dfp=d.get("false_positive", 0),
                        dfn=d.get("false_negative", 0),
                        rs=r.get("symbols_sampled", 0),
                        ras=r.get("available_symbols", 0),
                        rtp=r.get("true_positive", 0),
                        rfp=r.get("false_positive", 0),
                        rfn=r.get("false_negative", 0),
                        oracle=nav.get("rust_analyzer_lsp_ms", "?"),
                    )
                )
        write_query_table(out, report)
    return "\n".join(out) + "\n"


SUMMARIZERS = {
    "rust": rust_summary,
    "c-family": c_family_summary,
    "c": c_family_summary,
    "cpp": c_family_summary,
    "csharp": csharp_summary,
    "python": python_summary,
    "java": java_summary,
    "go": go_summary,
    "js-ts": js_ts_summary,
    "typescript": js_ts_summary,
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--language", required=True, choices=sorted(SUMMARIZERS))
    parser.add_argument("--report-glob", default="target/semantic-graph-benchmark/**/*.json")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()

    reports = load_reports(args.report_glob)
    if not reports:
        raise SystemExit(f"no benchmark reports matched {args.report_glob!r}")

    summary = SUMMARIZERS[args.language](reports)
    Path(args.output).write_text(summary, encoding="utf-8")
    print(summary, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
