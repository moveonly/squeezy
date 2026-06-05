#!/usr/bin/env python3
"""Combine v3 (loss cells) + v3w (win cells) squeezy results into the full board.
Usage: board_combined.py   (reads results-sqz-<tier>-v3.jsonl + -v3w.jsonl + rival)"""
import json
import os
from pathlib import Path

ALL = ["c","cpp","csharp","dart","go","java","js","kotlin","php",
       "python","ruby","rust","scala","swift","ts"]
HTH_ROOT = Path(os.environ.get("SQUEEZY_REALWORLD_SCRATCH", "/tmp/hth"))


def load(p):
    d = {}
    if Path(p).exists():
        for l in open(p):
            l = l.strip()
            if not l:
                continue
            r = json.loads(l)
            d[r["lang"]] = r
    return d


for tier, col in (("mini", "codex_cost"), ("haiku", "cc_cost")):
    sqz = load(HTH_ROOT / f"results-sqz-{tier}-v3.jsonl")
    sqz.update(load(HTH_ROOT / f"results-sqz-{tier}-v3w.jsonl"))
    riv = load(HTH_ROOT / f"results-rival-{tier}.jsonl")
    wins = 0
    csv_rows = [f"lang,sqz_wg_recall,sqz_wg_cost,{col},ratio,verdict"]
    print(f"\n=== {tier} v3 (full, vs rival n=3 medians) ===")
    missing = []
    for l in ALL:
        s, r = sqz.get(l), riv.get(l)
        if not s or not r or s.get("cost", 0) <= 0:
            print(f"  {l:8} PENDING/MISSING (sqz={s.get('cost') if s else None})")
            missing.append(l)
            continue
        sc, sr, rc, rr = s["cost"], s["recall"], r["cost"], r["recall"]
        ratio = sc / rc if rc else 0
        ok = (sr is not None and rr is not None and sr >= rr - 1e-9 and rc > 0 and ratio <= 0.95)
        v = "WIN" if ok else "LOSS"
        if ok:
            wins += 1
        tag = "" if ok else ("  <-- recall" if (sr is None or rr is None or sr < rr - 1e-9) else "  <-- cost")
        print(f"  {l:8} {v:4} ratio={ratio:.2f} sqz=${sc:.4f}({sr:.0f}) rival=${rc:.4f}({rr:.0f}){tag}")
        csv_rows.append(f"{l},{sr:.1f},{sc:.4f},{rc:.4f},{ratio:.2f},{v}")
    print(f"  WINS: {wins}/{15-len(missing)} measured ({len(missing)} pending: {missing})")
    (HTH_ROOT / f"board-{tier}-v3.csv").write_text("\n".join(csv_rows) + "\n")
