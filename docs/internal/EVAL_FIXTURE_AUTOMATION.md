# Eval fixture automation — design sketch

The four `multirepo-*.toml` fixtures in
`crates/squeezy-eval/fixtures/scenarios/` are 90% boilerplate around a
single shape:

> for a known public symbol in a target repo, ask the agent to find it,
> list nearby methods, and explain a relationship — each with confidence
> labels.

That shape is the natural unit of polyglot graph-nav coverage. Hand-
authoring one TOML per (repo, language) is fine for a handful; doing it
for every language × every common library is busywork. This doc sketches
how to lift it.

## Tiers

### Tier 1 — template + manual symbol picks

A single Tera/Handlebars template under
`crates/squeezy-eval/templates/graph-nav.toml.tera` driven by a YAML
roster:

```yaml
- id: multirepo-walkdir
  repo: BurntSushi/walkdir
  sha: 6fd031c82ba5a4204b4ce6eae73dacb00dc072ec
  language: rust
  symbol: WalkDir
  caller_symbol: WalkDir::new
- id: multirepo-typer
  repo: tiangolo/typer
  sha: 8d956dd26067fcefd664e94259de33b58ba8754a
  language: python
  symbol: Typer
  caller_symbol: Typer.command
```

A new subcommand `squeezy-eval generate --roster <yaml> --out <dir>`
expands the template once per row. ~150 LOC, no LLM calls, deterministic,
PR-reviewable. Lets a maintainer scale the roster without growing the
fixture surface.

### Tier 2 — auto-pick the symbol from the repo

The Tier 1 roster still requires a human to know which symbol matters
in each repo. Eliminate that by reading the repo's manifest:

| Language | Pick rule |
|---|---|
| Rust | first `pub struct \| pub trait \| pub enum` in `src/lib.rs` |
| Python | first `class` in the module declared in `pyproject.toml.project.name` |
| TypeScript | first `export class` in the entry file from `package.json.exports` |
| Go | first exported type in `<package>.go` |

Implementation reuses `squeezy_parse` and `squeezy_graph` — both are
already wired up to extract declarations per language. A new
`squeezy-eval generate --repo owner/name --sha <sha>` clones into a
tempdir, runs the picker, and emits a scenario TOML.

This is exactly the surface squeezy already implements; we'd be calling
it as a library from the eval generator.

### Tier 3 — LLM-assisted authoring

For repos where the picker gets it wrong (a domain library where the
"main symbol" is buried), run a tiny LLM call against the repo's README
+ top-of-`lib.rs`/`__init__.py` asking: _"name the 3 most central
public types"_. Use cheap-tier (Haiku / gpt-5.4-mini). Cost per
generated scenario: pennies. Determinism: lower; cap to dev-time use
unless you snapshot the picks.

This tier is opt-in via `--strategy llm` on the generator.

## Two follow-up subcommands worth shipping together

```
squeezy-eval generate \
    --roster docs/internal/eval-roster.yaml \
    --template templates/graph-nav.toml.tera \
    --out crates/squeezy-eval/fixtures/scenarios/

squeezy-eval matrix \
    --roster docs/internal/eval-roster.yaml \
    --baseline target/eval/baseline-<sha> \
    --fail-on findings,expectations
```

- `generate` is pure template expansion + repo provisioning. Deterministic.
- `matrix` runs every generated scenario against the live agent, diffs
  against a stored baseline, and exits non-zero on regressions. Wraps
  the existing `check` and `diff` subcommands; mostly orchestration.

`matrix` slots into CI behind a paid-API secret and would be the
"polyglot graph-nav nightly" gate.

## Estimated effort

- Tier 1: ~half a day. Template format + roster parser + emitter.
- Tier 2 picker: ~1 day. Most of it is teaching the generator how to
  read `Cargo.toml`/`pyproject.toml`/`package.json`/`go.mod`. The
  per-language entry-point detection is straightforward.
- Tier 3 LLM-assisted: ~half a day on top of Tier 2. Small prompt,
  reuse `squeezy_llm`.
- `matrix` orchestrator: ~half a day. Mostly composition of existing
  pieces.

## Recommendation

Ship Tier 1 + `matrix` first. That alone covers the polyglot-coverage
story (every language gets a fixture; rosters are PR-reviewable text
files; regressions get diffed by `matrix`). Add the auto-picker (Tier
2) when the roster crosses ~20 entries. Tier 3 only if a hard case
arises that the rules-based picker can't handle.

The four hand-authored `multirepo-*.toml` files this work produced
become the seed roster for Tier 1 — same content, expressed as YAML
rows instead of TOML files.
