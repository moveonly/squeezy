#!/usr/bin/env python3
"""Cost-breakdown analyzer for a squeezy eval run dir.

Prints per-component USD (raw-input/cache-read/cache-write/output), tool-call
count + per-tool breakdown, context-accumulation signal, and findings.
Usage: analyze.py <rundir>
"""
import json, sys
from collections import Counter
from pathlib import Path

H_IN, H_CR, H_CW, H_OUT = 1.00, 0.10, 1.25, 5.00      # haiku
M_IN, M_CR, M_OUT = 0.75, 0.075, 4.50                  # mini

rd = Path(sys.argv[1])
rj = json.loads((rd / "run.json").read_text())
model = rj.get("model", "")
haiku = "haiku" in model
ev = [json.loads(l) for l in (rd / "trace.jsonl").read_text().splitlines() if l.strip()]

inp = cr = cw = out = 0
turns = 0
for e in ev:
    if e.get("kind") == "turn_completed":
        p = (e.get("metrics") or {}).get("provider") or {}
        inp += p.get("input_tokens", 0) or 0
        cr += p.get("cached_input_tokens", 0) or 0
        cw += p.get("cache_write_input_tokens", 0) or 0
        out += p.get("output_tokens", 0) or 0
        turns += 1
raw = max(inp - cr - cw, 0)
if haiku:
    c_raw, c_cr, c_cw, c_out = raw*H_IN, cr*H_CR, cw*H_CW, out*H_OUT
else:
    c_raw, c_cr, c_cw, c_out = raw*M_IN, cr*M_CR, 0.0, out*M_OUT
tot = (c_raw + c_cr + c_cw + c_out) / 1e6

# tool calls (name from tool_call_started.call.name)
tools = Counter()
for e in ev:
    if e.get("kind") == "tool_call_started":
        tools[(e.get("call") or {}).get("name", "?")] += 1
ntool = sum(tools.values())

# narration size
deltas = sum(1 for e in ev if e.get("kind") == "assistant_delta")
reasoning = sum(1 for e in ev if e.get("kind") in ("reasoning_delta", "reasoning_segment"))

print(f"=== {rd.name} ({model}) ===")
print(f"cost ${tot:.4f}  turns={turns} tool_calls={ntool} assistant_deltas={deltas} reasoning={reasoning}")
print(f"tokens: input={inp} cache_read={cr} cache_write={cw} output={out} (raw_in={raw})")
print(f"  $ raw_in={c_raw/1e6:.4f}  cache_read={c_cr/1e6:.4f}  cache_write={c_cw/1e6:.4f}  output={c_out/1e6:.4f}")
pct = lambda x: f"{100*x/(tot*1e6):.0f}%" if tot else "0%"
print(f"  %: raw_in={pct(c_raw)} cache_read={pct(c_cr)} cache_write={pct(c_cw)} output={pct(c_out)}")
print(f"tool breakdown: {dict(tools.most_common())}")
for f in rj.get("findings", []):
    print(f"  FINDING: {f}")
