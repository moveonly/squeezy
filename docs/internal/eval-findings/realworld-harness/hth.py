#!/usr/bin/env python3
"""Head-to-head: squeezy vs rival on the SAME repo+question+grader, fair pricing.

Usage:
  hth.py <lang> <tier:mini|haiku> <side:squeezy|rival|both> <n> [variant:with-graph|no-graph]

- squeezy: runs the release squeezy-eval binary on the committed (mini) or
  generated (haiku) scenario toml. Cost = run.json totals.cost_micro_usd.
- rival mini  = codex exec -m gpt-5.4-mini ; cost priced 0.75/0.075/4.50 (== squeezy mini).
- rival haiku = claude --print --model haiku ; cost recomputed from usage at
  1.00/0.10/5.00/1.25 (== squeezy haiku), symmetric with squeezy's own method.
- Both graded by the SAME grade.GRADERS / corrected grade.GT.

Emits one JSON line per (side,rep) to stdout prefixed REC, plus a human summary.
"""
import json, os, re, statistics, subprocess, sys, time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HARNESS = Path(__file__).resolve().parent
REPO = HARNESS.parents[3]
sys.path.insert(0, str(HARNESS))
import grade
from grade import GRADERS, GT, cost_micro

NEW = Path(os.environ.get("SQUEEZY_REALWORLD_REPO", str(REPO))).resolve()
SC = NEW / "crates/squeezy-eval/fixtures/scenarios/benchmarks/natural"
HTH_ROOT = Path(os.environ.get("SQUEEZY_REALWORLD_SCRATCH", "/tmp/hth"))
HAIKU_TOML = Path(os.environ.get("SQUEEZY_REALWORLD_HAIKU_TOML", str(HTH_ROOT / "haiku-toml")))
REPOS = Path(os.environ.get("SQUEEZY_REALWORLD_REPOS", str(HTH_ROOT / "repos")))
BIN = os.environ.get("BIN", str(NEW / "target/release/squeezy-eval"))
CODEX_PROMPTS = Path(os.environ.get(
    "SQUEEZY_REALWORLD_CODEX_PROMPTS",
    str(HTH_ROOT / "prompts" / "codex"),
))
CC_PROMPTS = Path(os.environ.get(
    "SQUEEZY_REALWORLD_CC_PROMPTS",
    str(HTH_ROOT / "prompts" / "cc"),
))
REPOS.mkdir(parents=True, exist_ok=True)

CC_SCRUB = ["-u","CLAUDECODE","-u","CLAUDE_CODE_SESSION_ID","-u","CLAUDE_CODE_ENTRYPOINT",
            "-u","CLAUDE_CODE_EXECPATH","-u","AI_AGENT","-u","CLAUDE_EFFORT","-u","CLAUDE_CODE_TMPDIR"]

# Haiku per-Mtok rates (micro-USD/token), identical to squeezy models.json
H_IN, H_CR, H_CW, H_OUT = 1.00, 0.10, 1.25, 5.00


def repo_sha(lang):
    t = (SC / f"graph-vs-nograph-{lang}-realworld-with-graph.toml").read_text()
    m = re.search(r'\[workspace\.github\]\s*\nrepo = "([^"]+)"\s*\nsha = "([^"]+)"', t)
    if m:
        return m.group(1), m.group(2)
    return None, None  # local workspace (rust)


def ensure_repo(lang):
    """Clone repo@sha once into REPOS/<lang>; return the checkout dir. rust -> NEW."""
    repo, sha = repo_sha(lang)
    if repo is None:
        return NEW  # local = "." (rust)
    dest = REPOS / lang
    marker = dest / ".hth_ready"
    if marker.exists():
        return dest
    if dest.exists():
        subprocess.run(["rm", "-rf", str(dest)])
    subprocess.run(["git", "clone", "--no-checkout",
                    f"https://github.com/{repo}.git", str(dest)], check=True,
                   capture_output=True)
    subprocess.run(["git", "fetch", "--depth", "1", "origin", sha], cwd=dest,
                   check=True, capture_output=True)
    subprocess.run(["git", "checkout", sha], cwd=dest, check=True, capture_output=True)
    marker.write_text("ok")
    return dest


def grade_text(lang, text):
    g, gt = GRADERS.get(lang), GT.get(lang, {})
    if not (g and text and gt):
        return None, 0, 0
    try:
        f, tot = g(text, gt)
    except Exception:
        return None, 0, 0
    return (f / tot * 100.0 if tot else None), f, tot


# ---------------- squeezy ----------------
def run_squeezy(lang, tier, variant):
    if tier == "mini":
        toml = SC / f"graph-vs-nograph-{lang}-realworld-{variant}.toml"
    else:
        toml = HAIKU_TOML / f"{lang}-{variant}.toml"
    cap = 1500 if lang == "dart" else 700
    cmd = ["bash", "-lc",
           f"source /Users/abbassabra/.env.sh; cd {NEW} && timeout -k 30 {cap} "
           f"{BIN} run --quiet --out target/eval {toml}"]
    rundir = None
    for attempt in range(3):
        if attempt:
            time.sleep(40)
        p = subprocess.run(cmd, capture_output=True, text=True)
        m = re.search(r"eval run complete: (\S+)", p.stdout + p.stderr)
        if not m:
            continue
        rundir = m.group(1)
        rundir = rundir if Path(rundir).is_absolute() else str(NEW / rundir)
        rj_path = Path(rundir) / "run.json"
        if not rj_path.exists():
            continue
        rj = json.loads(rj_path.read_text())
        cost = rj.get("totals", {}).get("cost_micro_usd", 0) / 1e6
        if cost <= 0:
            continue
        ans = ""
        fp = Path(rundir) / "frames.jsonl"
        if fp.exists():
            for line in fp.read_text().splitlines():
                try:
                    fr = json.loads(line)
                    if fr.get("assistant_text"):
                        ans = fr["assistant_text"]
                except Exception:
                    pass
        recall, found, total = grade_text(lang, ans)
        return {"side": f"sqz-{tier}-{variant}", "lang": lang, "cost": cost,
                "recall": recall, "found": found, "total": total,
                "chars": len(ans), "rundir": rundir}
    return {"side": f"sqz-{tier}-{variant}", "lang": lang, "cost": 0.0,
            "recall": None, "found": 0, "total": 0, "chars": 0, "rundir": rundir,
            "error": "no-cost-run"}


# ---------------- codex (mini rival) ----------------
def run_codex(lang, repodir, rep):
    prompt = (CODEX_PROMPTS / f"{lang}.txt").read_text()
    ev = HTH_ROOT / "out" / f"codex-{lang}-r{rep}.jsonl"
    ev.parent.mkdir(parents=True, exist_ok=True)
    cap = 1500 if lang == "dart" else 700
    for attempt in range(3):
        if attempt:
            time.sleep(20)
        with open(ev, "w") as f:
            p = subprocess.run(
                ["bash", "-lc",
                 f"source /Users/abbassabra/.env.sh; timeout -k 30 {cap} codex exec --json "
                 f"--ignore-user-config --ephemeral --skip-git-repo-check "
                 f"-C {repodir} -m gpt-5.4-mini {json_q(prompt)} </dev/null"],
                stdout=f, stderr=subprocess.PIPE, text=True)
        usage, answer, turns = parse_codex(ev)
        if answer:
            cost = cost_micro(usage) / 1e6
            recall, found, total = grade_text(lang, answer)
            return {"side": "codex", "lang": lang, "cost": cost, "recall": recall,
                    "found": found, "total": total, "chars": len(answer),
                    "usage": usage, "turns": turns}
    return {"side": "codex", "lang": lang, "cost": 0.0, "recall": None, "found": 0,
            "total": 0, "chars": 0, "error": "empty"}


def parse_codex(ev):
    usage = {"input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0}
    answer, turns = "", 0
    for line in Path(ev).read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        t = o.get("type", "")
        if t == "turn.completed":
            u = o.get("usage", {})
            if u:
                usage = u  # last turn.completed = cumulative for the exec
                turns += 1
        elif t == "item.completed":
            it = o.get("item", {})
            if it.get("type") == "agent_message" or it.get("item_type") == "agent_message":
                if it.get("text"):
                    answer = it["text"]
    return usage, answer, turns


# ---------------- claude code (haiku rival) ----------------
def run_cc(lang, repodir, rep):
    prompt_file = CC_PROMPTS / f"{lang}.txt"
    stream = HTH_ROOT / "out" / f"cc-{lang}-r{rep}.jsonl"
    stream.parent.mkdir(parents=True, exist_ok=True)
    cap = 1500 if lang == "dart" else 700
    for attempt in range(3):
        if attempt:
            time.sleep(20)
        with open(stream, "w") as f:
            subprocess.run(
                ["env", *CC_SCRUB, "bash", "-lc",
                 f"cd {repodir} && timeout -k 30 {cap} claude --print --model haiku "
                 f"--output-format stream-json --verbose --bare "
                 f"--permission-mode bypassPermissions --tools Read Grep Glob Bash "
                 f"< {prompt_file}"],
                stdout=f, stderr=subprocess.DEVNULL, text=True)
        cost, answer, usage = parse_cc(stream)
        if answer:
            recall, found, total = grade_text(lang, answer)
            return {"side": "cc", "lang": lang, "cost": cost, "recall": recall,
                    "found": found, "total": total, "chars": len(answer), "usage": usage}
    return {"side": "cc", "lang": lang, "cost": 0.0, "recall": None, "found": 0,
            "total": 0, "chars": 0, "error": "empty"}


def parse_cc(stream):
    answer, cost, usage = "", 0.0, {}
    for line in Path(stream).read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        if o.get("type") == "result":
            answer = o.get("result", "") or answer
            mu = o.get("modelUsage", {})
            # recompute from per-model run totals (symmetric w/ squeezy token method)
            inp = cw = cr = out = 0
            for m in mu.values():
                inp += m.get("inputTokens", 0)
                cw += m.get("cacheCreationInputTokens", 0)
                cr += m.get("cacheReadInputTokens", 0)
                out += m.get("outputTokens", 0)
            if not mu:  # fallback to result.usage
                u = o.get("usage", {})
                inp = u.get("input_tokens", 0); cw = u.get("cache_creation_input_tokens", 0)
                cr = u.get("cache_read_input_tokens", 0); out = u.get("output_tokens", 0)
            cost = (inp * H_IN + cr * H_CR + cw * H_CW + out * H_OUT) / 1e6
            usage = {"inp": inp, "cr": cr, "cw": cw, "out": out,
                     "reported_usd": o.get("total_cost_usd")}
    return cost, answer, usage


def json_q(s):
    """shell-safe single-arg quoting for the prompt."""
    return "'" + s.replace("'", "'\\''") + "'"


def main():
    lang, tier, side, n = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
    variant = sys.argv[5] if len(sys.argv) > 5 else "with-graph"
    repodir = ensure_repo(lang)
    jobs = []
    for rep in range(n):
        if side in ("squeezy", "both"):
            jobs.append(("sqz", rep))
        if side in ("rival", "both"):
            jobs.append(("rival", rep))
    maxw = int(os.environ.get("MAXW", "2"))
    results = []

    def do(job):
        kind, rep = job
        if kind == "sqz":
            return run_squeezy(lang, tier, variant)
        if tier == "mini":
            return run_codex(lang, repodir, rep)
        return run_cc(lang, repodir, rep)

    with ThreadPoolExecutor(max_workers=maxw) as ex:
        for r in ex.map(do, jobs):
            results.append(r)
            print("REC " + json.dumps(r))
            sys.stdout.flush()

    # summary
    def med(side_results, key):
        vals = [r[key] for r in side_results if r.get(key) and r[key] > 0]
        return statistics.median(vals) if vals else 0.0
    sqz = [r for r in results if r["side"].startswith("sqz")]
    riv = [r for r in results if r["side"] in ("codex", "cc")]
    print(f"\n==== {lang} {tier} {variant} (n={n}) ====")
    for label, group in (("squeezy", sqz), ("rival", riv)):
        if not group:
            continue
        costs = sorted(r["cost"] for r in group if r["cost"] > 0)
        recalls = [r["recall"] for r in group if r["recall"] is not None]
        print(f"  {label:8} cost={[f'{c:.4f}' for c in costs]} med=${med(group,'cost'):.4f} "
              f"recall={recalls}")
    if sqz and riv:
        sc, rc = med(sqz, "cost"), med(riv, "cost")
        sr = statistics.median([r["recall"] for r in sqz if r["recall"] is not None] or [0])
        rr = statistics.median([r["recall"] for r in riv if r["recall"] is not None] or [0])
        ratio = sc / rc if rc else 0
        verdict = "WIN" if (sr >= rr - 1e-9 and ratio < 0.95 and rc > 0) else (
                  "LOSS" if (sr < rr - 1e-9 or ratio > 1.0) else "CLOSE")
        print(f"  VERDICT {verdict}  ratio={ratio:.2f}  sqz_recall={sr} rival_recall={rr}")
        print("SUMMARY " + json.dumps({"lang": lang, "tier": tier, "variant": variant,
              "sqz_cost": sc, "rival_cost": rc, "sqz_recall": sr, "rival_recall": rr,
              "ratio": ratio, "verdict": verdict}))


if __name__ == "__main__":
    main()
