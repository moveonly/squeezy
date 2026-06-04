#!/usr/bin/env python3
"""Grade each codex answer against ground truth.

For each run:
- extract codex's answer rows
- count overlap with ground-truth file-basename + key (line or class name)
- recall = found / total
- cost from final usage event
"""
import json
import re
import os
import sys
from pathlib import Path

OUT = Path("/tmp/codex-runs/realworld")
RES = OUT / "results.json"

# gpt-5.4-mini pricing in micro-USD per token
P_IN = 0.75   # per million
P_CACHED = 0.075
P_OUT = 4.50

GT = json.loads((OUT / "ground_truth.json").read_text())


def cost_micro(usage):
    inp = usage.get("input_tokens", 0) or 0
    cached = usage.get("cached_input_tokens", 0) or 0
    out = usage.get("output_tokens", 0) or 0
    raw_inp = max(inp - cached, 0)
    micro = raw_inp * P_IN + cached * P_CACHED + out * P_OUT
    return int(round(micro))


def extract_usage_and_tools(events_path):
    """Return (usage_dict, tool_call_count, trace_event_count)."""
    usage = {"input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0, "reasoning_output_tokens": 0}
    tool_calls = 0
    trace_events = 0
    final_text = ""
    try:
        with open(events_path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                trace_events += 1
                try:
                    obj = json.loads(line)
                except Exception:
                    continue
                t = obj.get("type", "")
                if t == "turn.completed":
                    u = obj.get("usage", {})
                    if u:
                        usage = u
                elif t == "item.completed":
                    it = obj.get("item", {})
                    if it.get("type") in ("command_execution", "tool_call", "mcp_tool_call"):
                        tool_calls += 1
                    if it.get("type") == "agent_message" or it.get("item_type") == "agent_message":
                        txt = it.get("text", "")
                        if txt:
                            final_text = txt
    except FileNotFoundError:
        pass
    return usage, tool_calls, trace_events, final_text


def file_basenames_from_text(text):
    """Extract file paths/basenames mentioned in answer text."""
    paths = set()
    # match path/x/y.ext patterns
    for m in re.finditer(r"[A-Za-z0-9_/\\.\\-]+\\.(?:rs|cpp|h|cs|dart|go|java|js|kt|php|py|rb|scala|swift)", text):
        paths.add(m.group(0))
    return paths


def grade_by_sites(text, sites):
    """Match codex answer rows (file_basename + line or class) against ground-truth sites."""
    # sites = list of (path, line) or (path, line, key)
    found = set()
    for site in sites:
        path = site[0]
        line = site[1] if len(site) > 1 else None
        bn = os.path.basename(path)
        # Look for path or basename in text; also accept ±1 on line
        if path in text or bn in text:
            # If we have a line, require the line number ±2 also nearby (within 200 chars)
            if line is not None:
                # find each occurrence of bn and check nearby
                ok = False
                for m in re.finditer(re.escape(bn), text):
                    window = text[max(0, m.start()-40):m.end()+80]
                    nums = [int(x) for x in re.findall(r"\b(\d{1,5})\b", window)]
                    if any(abs(n - line) <= 2 for n in nums):
                        ok = True
                        break
                if ok:
                    found.add((path, line))
                else:
                    # accept path-only match if a class/key string is also present
                    if len(site) > 2 and site[2] and site[2] in text:
                        found.add((path, line))
            else:
                found.add((path,))
    return found


def grade_by_keys(text, keys):
    """Match codex answer rows by string keys (class names, function names)."""
    found = set()
    for k in keys:
        if re.search(r"\b" + re.escape(k) + r"\b", text):
            found.add(k)
    return found


def grade_rust(text, gt):
    """Drift-proof grader for the `impl LlmProvider` classification task.

    The rust scenario runs against the LOCAL working tree (`local = "."`),
    so the `impl` line numbers drift between checkouts and a `line +/- 2`
    match spuriously fails correct answers. The provider TYPE names are
    unique and are a required output field, so match each production
    `impl LlmProvider for <Type>` by its type name appearing in the
    answer (same presence approach as grade_java).
    """
    sites = gt["sites"]
    found = 0
    for site in sites:
        ty = site[2] if len(site) > 2 else None
        if ty and re.search(r"\b" + re.escape(ty) + r"\b", text):
            found += 1
    return found, len(sites)


def grade_cpp(text, gt):
    sites = gt["sites"]
    found = grade_by_sites(text, sites)
    return len(found), len(sites)


def grade_csharp(text, gt):
    sites = gt["sites"]
    # site = (rel, line, method_name)
    # We want to count each (file, line, method) triple represented.
    found = 0
    for path, line, method in sites:
        bn = os.path.basename(path)
        # find file mentions
        for m in re.finditer(re.escape(bn), text):
            window = text[max(0, m.start()-100):m.end()+200]
            if re.search(r"\b" + re.escape(method) + r"\b", window):
                nums = [int(x) for x in re.findall(r"\b(\d{1,5})\b", window)]
                if any(abs(n - line) <= 2 for n in nums):
                    found += 1
                    break
    return found, len(sites)


def grade_dart(text, gt):
    sites = gt["sites"]
    # site = (rel, line, class)
    found = 0
    for path, line, cls in sites:
        if cls and re.search(r"\b" + re.escape(cls) + r"\b", text):
            found += 1
    return found, len(sites)


def grade_go(text, gt):
    methods = gt["methods"]
    # methods = {name: [doc files]}
    # Need codex to emit each method name
    found = 0
    for name in methods:
        if re.search(r"\b" + re.escape(name) + r"\b", text):
            found += 1
    return found, len(methods)


def grade_java(text, gt):
    sites = gt["sites"]
    # site = (rel, line, class)
    found = 0
    for path, line, cls in sites:
        if cls and re.search(r"\b" + re.escape(cls) + r"\b", text):
            found += 1
    return found, len(sites)


def grade_js(text, gt):
    rows = gt["rows"]
    # row = (fp-name, kind, root.js)
    found = 0
    for name, kind, rt in rows:
        # Look for `<name> <kind> <rt>` triple (allowing flexible whitespace)
        pat = r"\b" + re.escape(name) + r"\s+" + re.escape(kind) + r"\s+" + re.escape(rt)
        if re.search(pat, text):
            found += 1
        elif re.search(r"\b" + re.escape(name) + r"\b", text) and re.search(r"\b" + re.escape(rt) + r"\b", text):
            # partial credit: name and root present in same answer (less strict)
            found += 1
    return found, len(rows)


def grade_kotlin(text, gt):
    rows = gt["rows"]
    found = 0
    for path, line, cls in rows:
        if re.search(r"\b" + re.escape(cls) + r"\b", text):
            found += 1
    return found, len(rows)


def grade_php(text, gt):
    rows = gt["rows"]
    found = 0
    for path, line, cls in rows:
        if re.search(r"\b" + re.escape(cls) + r"\b", text):
            found += 1
    return found, len(rows)


def grade_python(text, gt):
    """Grade the psf/requests subclass-surface inventory.

    The committed prompt (revised in #201) asks for every class under
    `src/requests/` and `tests/` whose DIRECT base names one of the core
    hierarchy bases, paired with the non-dunder methods it defines
    directly. One line per `(path, class)`:

        <repo-relative-path>::<C>: <comma-sorted non-dunder methods, or (none)>

    GT rows are keyed by `<path>::<Class>` -> sorted method list, computed
    from the pinned SHA via ast. A row counts as found when its parsed
    method SET exactly equals the expected set.
    """
    rows = gt["rows"]
    total = len(rows)
    parsed = {}
    # De-anchored: find every `<path>.py::<Class>: <methods>` row wherever it
    # occurs in the text, not only at line start. Some models stream the first
    # data row glued to a prose preamble with no preceding newline (e.g.
    # `...extract the information:src/requests/_types.py::Foo: (none)`); a
    # line-anchored regex silently dropped that correct row. The key is
    # anchored on `.py::` so this cannot match arbitrary prose `::` text, and
    # the body is cut before any next row-key glued onto the same line.
    key_pat = r"[\w./]+\.py::[A-Za-z_]\w*"
    for m in re.finditer(r"(" + key_pat + r")\s*:\s*([^\n]*)", text):
        key = m.group(1).lstrip("./")
        body = m.group(2)
        nxt = re.search(key_pat + r"\s*:", body)
        if nxt:
            body = body[:nxt.start()]
        body = body.strip()
        if body in ("(none)", "none", "()", ""):
            methods = set()
        else:
            body = re.split(r"\s*#\s*", body, maxsplit=1)[0].strip()
            methods = {c.strip().strip("`") for c in body.split(",")
                       if c.strip() and c.strip() != "(none)"}
        parsed[key] = methods
    found = 0
    for key, expected in rows.items():
        k = key.lstrip("./")
        if k in parsed and parsed[k] == set(expected):
            found += 1
    return found, total


def grade_ruby(text, gt):
    rows = gt["rows"]
    found = 0
    seen_classes = set()
    for path, line, fqn in rows:
        # extract short class name (last segment) from fqn
        # ruby fqn may have :: prefix
        simple = fqn.split("::")[-1]
        if re.search(r"\b" + re.escape(simple) + r"\b", text):
            found += 1
            seen_classes.add(simple)
    return found, len(rows)


def grade_scala(text, gt):
    rows = gt["rows"]
    found = 0
    for path, line, cls in rows:
        if re.search(r"\b" + re.escape(cls) + r"\b", text):
            found += 1
    return found, len(rows)


def grade_swift(text, gt):
    rows = gt["rows"]
    # Score by file+line proximity since names repeat (get/post/put etc.)
    found = 0
    for path, line, name in rows:
        bn = os.path.basename(path)
        ok = False
        for m in re.finditer(re.escape(bn), text):
            window = text[max(0, m.start()-60):m.end()+120]
            if re.search(r"\b" + re.escape(name) + r"\b", window):
                nums = [int(x) for x in re.findall(r"\b(\d{1,5})\b", window)]
                if any(abs(n - line) <= 2 for n in nums):
                    ok = True
                    break
        if ok:
            found += 1
    return found, len(rows)


def grade_c(text, gt):
    """Grade nginx push-sites: each row is (module_file, module_var, postconfig_fn, phase, handler_fn).

    A row is counted as found when the module file's basename, the phase
    enumerator, and the handler identifier all co-occur within a 240-char
    window in the answer text. We additionally check that the module_var
    or postconfig_fn appears in the same window so we do not give
    credit for naive grep dumps that name the file but never tie the
    phase to its handler.
    """
    rows = gt["rows"]
    found = 0
    for module_file, module_var, postconfig_fn, phase, handler_fn in rows:
        bn = os.path.basename(module_file)
        ok = False
        # try basename then full path occurrences
        for needle in (module_file, bn):
            for m in re.finditer(re.escape(needle), text):
                window = text[max(0, m.start() - 40):m.end() + 240]
                if (phase in window
                        and re.search(r"\b" + re.escape(handler_fn) + r"\b", window)
                        and (module_var in window or postconfig_fn in window)):
                    ok = True
                    break
            if ok:
                break
        if ok:
            found += 1
    return found, len(rows)


def grade_ts(text, gt):
    """Grade nest microservices subclass list.

    Each row is (subclass, base, type_args, file). Subclass names are
    unique, so we count a row found iff the subclass name appears AND
    a sentinel token from the type_args appears within the same
    line/segment as the subclass.
    """
    rows = gt["rows"]
    found = 0
    for subclass, base, type_args, _file in rows:
        # find subclass occurrences, then check that the first
        # type-arg identifier (or `never`/`E`) appears nearby on the
        # same line or within ~80 chars after.
        first_arg = type_args.split(",")[0].strip()
        ok = False
        for m in re.finditer(r"\b" + re.escape(subclass) + r"\b", text):
            window = text[m.start():m.end() + 200]
            if base in window and re.search(r"\b" + re.escape(first_arg) + r"\b", window):
                ok = True
                break
        if ok:
            found += 1
    return found, len(rows)


GRADERS = {
    "rust": grade_rust,
    "cpp": grade_cpp,
    "csharp": grade_csharp,
    "dart": grade_dart,
    "go": grade_go,
    "java": grade_java,
    "js": grade_js,
    "kotlin": grade_kotlin,
    "php": grade_php,
    "python": grade_python,
    "ruby": grade_ruby,
    "scala": grade_scala,
    "swift": grade_swift,
    "c": grade_c,
    "ts": grade_ts,
}


def main():
    results = {}
    langs = ["rust", "cpp", "csharp", "dart", "go", "java", "js", "kotlin", "php", "python", "ruby", "scala", "swift"]
    for lang in langs:
        gt = GT.get(lang, {})
        per_run = []
        for r in (1, 2, 3):
            ev = OUT / f"{lang}-r{r}.events.jsonl"
            if not ev.exists():
                per_run.append({"run": r, "status": "missing"})
                continue
            usage, tool_calls, trace_events, final_text = extract_usage_and_tools(ev)
            ans_path = OUT / f"{lang}-r{r}.answer.txt"
            if final_text:
                ans_path.write_text(final_text)
            cost_u = cost_micro(usage)
            grader = GRADERS.get(lang)
            found, total = 0, 0
            if grader and final_text:
                try:
                    found, total = grader(final_text, gt)
                except Exception as e:
                    found, total = 0, len(gt.get("sites") or gt.get("rows") or gt.get("methods") or [])
            recall_pct = (found / total * 100) if total else 0.0
            per_run.append({
                "run": r,
                "status": "ok" if final_text else "empty",
                "input_tokens": usage.get("input_tokens", 0),
                "cached_input_tokens": usage.get("cached_input_tokens", 0),
                "output_tokens": usage.get("output_tokens", 0),
                "reasoning_output_tokens": usage.get("reasoning_output_tokens", 0),
                "cost_micro_usd": cost_u,
                "cost_usd": cost_u / 1_000_000.0,
                "tool_calls": tool_calls,
                "trace_events": trace_events,
                "recall_found": found,
                "recall_total": total,
                "recall_pct": round(recall_pct, 1),
                "answer_chars": len(final_text),
            })
        results[lang] = per_run
    RES.write_text(json.dumps(results, indent=2))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
