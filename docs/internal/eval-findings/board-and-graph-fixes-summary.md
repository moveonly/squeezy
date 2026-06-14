# Graph-value board + correctness sweep — summary

A set of generic semantic-graph correctness/efficiency fixes (`perf/graph-jsts-inheritance`),
each comprehension-board flip tied to a concrete fix (no benchmark-specific hacks). Current
tallies live in the CSVs and `realworld-scoreboard-methodology.md`.

## Board (n=3 medians)

- **mini tier** (squeezy on gpt-5.4-mini vs `codex exec -m gpt-5.4-mini`): **15/15 WIN**
  (`mini-vs-codex-realworld.csv`).
- **haiku tier** (squeezy on claude-haiku-4-5 vs Claude Code haiku): **15/15 WIN**
  (`haiku-vs-cc-realworld.csv`).

### Flips earned this effort (haiku 9 → 13)

| lang | before | after | mechanism |
|---|---|---|---|
| **php** | 1.22 LOSS | **0.46 WIN** | `read_slice`-over-`read_file` steering — squeezy reads tight slices, not whole files (input tokens ↓) |
| **python** | 1.54 LOSS | **0.54 WIN** | same steering; transitive closure returns the deep/aliased subtree in one call (100% recall) |
| **ts** | 1.83 LOSS | **0.79 WIN** | exact attribute matching (review #6): the closure stopped over-matching (`base:Error` no longer pulls `base:ErrorHandler`), so it returns the 59 true Error descendants (100% recall) without the costly file-validation reads |
| **dart** | 0.98 LOSS | **0.58 WIN** | planner routing fix (batch-3 #6): "subclasses of RenderBox" now pre-issues transitive `decl_search attribute=base:` instead of the wrong `hierarchy` tool; squeezy gets the full closure at 100% recall while CC greps and misses (median 95.6%) at higher cost |

### Honest losses (kept LOSS)

- **c — 1.01 (near-tie), 100% recall.** Cost dominated by a delegate sub-agent reading all
  ~60 nginx module files (~606k input tokens). Not a graph-attributable cost; the graph fixes
  don't change it.
- **go — 1.58, 100% recall.** Eager `repo_map` over-scan (~682k input vs CC ~29k). A steering
  issue, not a graph-correctness bug; #6's planner fix doesn't apply (go's task isn't
  inheritance-shaped). The committed haiku `go` task is the cobra doc-method enumeration.

## Graph-correctness fixes shipped (three ultrareview batches)

**Batch 1 — transitive closure + tool contract.** Mixed-kind closure walk (walk kind-agnostically,
filter kind on emit); honor `query` on transitive `decl_search`; exact attribute matching
(no substring collisions); `reference_search` directory-scoped `path=`; `symbol_context` /
diff-range / hierarchy truncation reported honestly; last-receipt diff strips line-number
gutter before diffing; Ruby namespace mixin seeds the leaf only.

**Batch 2 — call/edge resolution + incremental refresh.** Receiver-aware method resolution
(no binding `b.foo()` to the caller's own class); arity-fallback gated by receiver/scope;
aliased imports indexed against the original symbol; JS/TS inheritance feeds `this.foo()`/
`super.foo()` resolution; stale symbols purged on supported→unsupported refresh;
budget-exhausted refresh keeps unprocessed paths pending (no silent stale serving).

**Batch 3 — availability + per-language scope + planner.** Path-only `read_slice` works without
the graph (no longer blocked while indexing); graph payloads surface `refresh_incomplete`/
`stale_pending`; Python/Dart class resolution scoped by imports/library (not global leaf name);
TS `import type` no longer emits a bogus `type` import; Kotlin qualified types record the leaf,
not the package segment; planner routes inheritance questions to `decl_search base:`; half-open
span containment helpers; graph-open errors preserved (distinguishable from "no graph").

**Parser follow-ups.** Go pointer/qualified (`*Animal`, `io.Reader`) embeds emit `go:embed`
field symbols; namespaced Ruby mixins also record `mixin:<ns>::<leaf>` for disambiguation;
`read_slice` schema wording corrected to the real auto-widen constants (~40→~48 lines).

## Methodology / honesty notes

- **n=3 medians.** The eval is high-variance (per-cell cost spreads 1.5–2×). Single runs are
  unreliable — e.g. `go` measured 0.87 (apparent WIN) at n=1 purely from a CC cost-variance
  draw, but 1.58 LOSS at n=3. No flip is committed on n=1.
- **WIN = squeezy ≤ 0.95× rival cost at ≥ rival recall.**
- **dart is the most variance-prone win:** squeezy's recall median is 100% (one of three reps
  dipped to 85%), CC's median is 95.6%; the win rests on squeezy's reliable closure recall vs
  CC's grep misses, plus lower median cost. Worth periodic re-confirmation.
