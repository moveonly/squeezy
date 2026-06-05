#!/usr/bin/env python3
"""Regenerate rival prompts + Haiku tomls from the CURRENT committed scenarios.

Eliminates the java/python/php prompt drift found in the audit: rival prompt ==
the exact `[[steps]] text` the squeezy scenario uses. Haiku toml == committed
with-graph/no-graph toml with only provider/model/id swapped to Anthropic Haiku.
"""
import os
import re
import sys
from pathlib import Path

HARNESS = Path(__file__).resolve().parent
REPO = HARNESS.parents[3]
NEW = Path(os.environ.get("SQUEEZY_REALWORLD_REPO", str(REPO))).resolve()
SC = NEW / "crates/squeezy-eval/fixtures/scenarios/benchmarks/natural"
LANGS = ["c","cpp","csharp","dart","go","java","js","kotlin","php",
         "python","ruby","rust","scala","swift","ts"]

HTH_ROOT = Path(os.environ.get("SQUEEZY_REALWORLD_SCRATCH", "/tmp/hth"))
CODEX_PROMPTS = Path(os.environ.get(
    "SQUEEZY_REALWORLD_CODEX_PROMPTS",
    str(HTH_ROOT / "prompts" / "codex"),
))
CC_PROMPTS = Path(os.environ.get(
    "SQUEEZY_REALWORLD_CC_PROMPTS",
    str(HTH_ROOT / "prompts" / "cc"),
))
HAIKU_TOML = Path(os.environ.get("SQUEEZY_REALWORLD_HAIKU_TOML", str(HTH_ROOT / "haiku-toml")))
for d in (CODEX_PROMPTS, CC_PROMPTS, HAIKU_TOML):
    d.mkdir(parents=True, exist_ok=True)


def step_text(toml_text):
    m = re.search(r'kind\s*=\s*"prompt"\s*\ntext\s*=\s*"""(.*?)"""', toml_text, re.S)
    if not m:
        raise SystemExit("no prompt step found")
    return m.group(1).strip() + "\n"


def make_haiku(toml_text, lang, variant):
    t = toml_text
    t = re.sub(r'^id = "([^"]+)"', lambda m: f'id = "{m.group(1)}-haiku"', t, count=1, flags=re.M)
    t = re.sub(r'^provider = "[^"]+"', 'provider = "anthropic"', t, count=1, flags=re.M)
    t = re.sub(r'^model = "[^"]+"', 'model = "claude-haiku-4-5-20251001"', t, count=1, flags=re.M)
    return t


for lang in LANGS:
    wg = (SC / f"graph-vs-nograph-{lang}-realworld-with-graph.toml").read_text()
    ng = (SC / f"graph-vs-nograph-{lang}-realworld-no-graph.toml").read_text()
    prompt = step_text(wg)
    (CODEX_PROMPTS / f"{lang}.txt").write_text(prompt)
    (CC_PROMPTS / f"{lang}.txt").write_text(prompt)
    (HAIKU_TOML / f"{lang}-with-graph.toml").write_text(make_haiku(wg, lang, "with-graph"))
    (HAIKU_TOML / f"{lang}-no-graph.toml").write_text(make_haiku(ng, lang, "no-graph"))
    print(f"{lang:8} prompt={len(prompt)}c  haiku tomls written")

print("done")
