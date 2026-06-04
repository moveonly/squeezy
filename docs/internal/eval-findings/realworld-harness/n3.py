#!/usr/bin/env python3
"""Efficient n=k measurement with rival/squeezy separation (rival reused across squeezy iters).

Modes:
  n3.py rival   <tier> <n> [langs] [lc]   -> run rival n times/lang, save medians to results-rival-<tier>.jsonl
  n3.py squeezy <tier> <n> <label> [langs] [lc] -> run squeezy n/lang, save to results-sqz-<tier>-<label>.jsonl
  n3.py verdict <tier> <label>             -> combine squeezy(label) + rival medians -> board
"""
import json, os, statistics, subprocess, sys
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ALL = ["c","cpp","csharp","dart","go","java","js","kotlin","php",
       "python","ruby","rust","scala","swift","ts"]
HTH = "/tmp/hth/hth.py"


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def med_pos(xs):
    xs = [x for x in xs if x and x > 0]
    return statistics.median(xs) if xs else 0.0


def run_side(lang, tier, side, n):
    p = subprocess.run(["python3", HTH, lang, tier, side, str(n)],
                       capture_output=True, text=True, env=dict(os.environ, MAXW="2"))
    costs, recs = [], []
    want = {"squeezy": ("sqz",), "rival": ("codex", "cc")}[side]
    for line in p.stdout.splitlines():
        if line.startswith("REC "):
            r = json.loads(line[4:])
            if r["side"].startswith(want) or r["side"] in want:
                costs.append(r.get("cost", 0))
                recs.append(r.get("recall"))
    return {"lang": lang, "cost": med_pos(costs), "recall": med(recs),
            "costs": costs, "recalls": recs, "n_ok": sum(1 for c in costs if c > 0)}


def sweep(tier, side, n, langs, lc, outfile):
    Path(outfile).write_text("")
    print(f"=== {side} {tier} n={n} langs={len(langs)} lc={lc} -> {outfile} ===", flush=True)
    with ThreadPoolExecutor(max_workers=lc) as ex:
        futs = {ex.submit(run_side, l, tier, side, n): l for l in langs}
        from concurrent.futures import as_completed
        for fut in as_completed(futs):
            r = fut.result()
            with open(outfile, "a") as f:
                f.write(json.dumps(r) + "\n")
            print(f"  {r['lang']:8} ${r['cost']:.4f} recall={r['recall']} "
                  f"(n_ok={r['n_ok']}/{n}) costs={['%.4f'%c for c in r['costs'] if c>0]} rec={r['recalls']}", flush=True)


def main():
    mode = sys.argv[1]
    if mode in ("rival", "squeezy"):
        tier, n = sys.argv[2], int(sys.argv[3])
        if mode == "squeezy":
            label = sys.argv[4]; rest = sys.argv[5:]
            outfile = f"/tmp/hth/results-sqz-{tier}-{label}.jsonl"
        else:
            rest = sys.argv[4:]
            outfile = f"/tmp/hth/results-rival-{tier}.jsonl"
        langs = (rest[0].split(",") if rest and rest[0] != "all" else ALL)
        lc = int(rest[1]) if len(rest) > 1 else (3 if tier == "mini" else 2)
        sweep(tier, mode, n, langs, lc, outfile)
    elif mode == "verdict":
        tier, label = sys.argv[2], sys.argv[3]
        sqz = {json.loads(l)["lang"]: json.loads(l) for l in open(f"/tmp/hth/results-sqz-{tier}-{label}.jsonl")}
        riv = {json.loads(l)["lang"]: json.loads(l) for l in open(f"/tmp/hth/results-rival-{tier}.jsonl")}
        wins = 0
        print(f"=== VERDICT {tier} (sqz={label} vs rival medians) ===")
        for l in ALL:
            s, r = sqz.get(l), riv.get(l)
            if not s or not r:
                print(f"  {l:8} MISSING"); continue
            sc, sr, rc, rr = s["cost"], s["recall"], r["cost"], r["recall"]
            ok = (sr is not None and rr is not None and sr >= rr - 1e-9 and rc > 0 and sc < rc * 0.95)
            v = "WIN" if ok else "LOSS"
            if ok: wins += 1
            ratio = sc/rc if rc else 0
            print(f"  {l:8} {v:4} ratio={ratio:.2f} sqz=${sc:.4f}({sr}) rival=${rc:.4f}({rr})")
        print(f"  WINS: {wins}/15")


if __name__ == "__main__":
    main()
