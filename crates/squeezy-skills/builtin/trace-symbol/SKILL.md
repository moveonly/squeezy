---
name: trace-symbol
description: Trace a symbol through Squeezy's semantic graph — definition, references, type and containment hierarchy, and call flow — before reading raw source.
when_to_use: When the user asks where a symbol is defined, who calls it, what it calls, its subtypes or members, or the blast radius of changing it.
triggers:
  - who calls
  - what calls
  - find references
  - call graph
  - trace symbol
  - blast radius
---

# Trace Symbol

Answer "where / who / what / impact" questions about a symbol with the
semantic graph tools first, and fall back to raw reads only to confirm exact
lines. The graph follows imports, re-exports, and renamed aliases that `grep`
misses, and each tool returns a compact packet instead of a whole file — so
the answer stays cheap. Per the repo principle, raw file reads are the last
step, not the first.

## Which tool for which question

- `repo_map` — orient in an unfamiliar workspace: hierarchy, language counts,
  and graph coverage. Run once at the start when you do not know the layout.
- `definition_search` — "where is X defined?". The best first tool to resolve
  a name to its defining file and a `symbol_id`. Use it before the flow tools
  when a name may be ambiguous.
- `decl_search` — broad lists or counts by name, kind, language, path, or
  visibility. For subtypes pass `attribute="base:<Type>"` (extends),
  `iface:<Type>` (implements), or `mixin:<Type>` (Dart `with`); add
  `transitive=true` for the full subtype closure.
- `reference_search` — "find every reference to X". One call returns every
  callsite across the graph; prefer it over N greps for the same name.
- `hierarchy` — containment (file → module → class → members). Call
  `hierarchy(symbol_id=<class>)` to enumerate a type's members before reading
  any bodies. This is containment, NOT inheritance — use `decl_search` with an
  inheritance `attribute` for subclasses.
- `upstream_flow` — "who calls X, within N hops?". A bounded caller search.
- `downstream_flow` — "what does X call?". A bounded callee search; pass
  `target_query` to get an explicit call chain between two symbols.
- `symbol_context` — one packet bundling callers, callees, references, and
  diff annotations for a query. Use it for relationship or impact questions;
  skip it for a plain definition lookup that `definition_search` answers.
- `read_slice` — read an exact bounded slice by `symbol_id` (with
  `span_kind=body`), line range, or path and offset. The last step, only after
  the graph has located the target.

## Recipe: locate and understand a symbol

1. If the workspace is unfamiliar, call `repo_map` once to orient.
2. `definition_search query="<name>"` to resolve the definition and capture its
   `symbol_id`. If several candidates come back, narrow with `path=` or `kind=`.
3. Reuse that `symbol_id` (not the bare name) for follow-ups so the graph stays
   on the resolved symbol:
   - callers → `upstream_flow symbol_id="<id>"`
   - callees → `downstream_flow symbol_id="<id>"`
   - all references → `reference_search symbol_id="<id>"`
   - members of a type → `hierarchy symbol_id="<id>"`
4. Only now `read_slice symbol_id="<id>" span_kind=body` to read the exact
   definition, or read a returned callsite's range to inspect one use.

## Recipe: blast radius before an edit

1. `symbol_context query="<name>"` for a single packet of callers, callees, and
   references, or `upstream_flow` to enumerate everything that depends on the
   symbol you are about to change.
2. For uncommitted work, `diff_context` returns the current Git change set with
   graph cross-references — use it for "what does this diff affect?".
3. Read only the impacted slices with `read_slice`; do not re-read whole files.

## Notes

- Pass a `symbol_id` from a previous graph packet whenever you have one — it is
  unambiguous, where a bare `query` may re-match several declarations.
- Unsupported file types return empty graph packets; fall back to `grep` or
  `read_file` for those, and for literal-text searches the graph does not index.
