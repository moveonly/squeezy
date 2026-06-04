#!/usr/bin/env python3
"""Regenerate rival prompts + Haiku tomls from the CURRENT committed scenarios.

Eliminates the java/python/php prompt drift found in the audit: rival prompt ==
the exact `[[steps]] text` the squeezy scenario uses. Haiku toml == committed
with-graph/no-graph toml with only provider/model/id swapped to Anthropic Haiku.
"""
import re, sys
from pathlib import Path

NEW = Path("/Users/abbassabra/esqueezy/new")
SC = NEW / "crates/squeezy-eval/fixtures/scenarios/benchmarks/natural"
LANGS = ["c","cpp","csharp","dart","go","java","js","kotlin","php",
         "python","ruby","rust","scala","swift","ts"]

CODEX_PROMPTS = Path("/tmp/codex-runs/realworld/prompts")
CC_PROMPTS = Path("/tmp/cc-baseline-realworld/prompts")
HAIKU_TOML = Path("/tmp/hth/haiku-toml")
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
