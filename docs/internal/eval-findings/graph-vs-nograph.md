# Graph vs. no-graph eval — realworld multi-step audits

A controlled A/B of squeezy with its semantic-graph tool family
available against the same agent loop with that family hidden, exercised
against twelve real-world multi-step refactor / audit prompts (one per
language) rooted in upstream OSS repos. Each scenario asks for a
structured per-row classification (override strategy, mixin set,
diagnostic mode, setter scope), not just an enumeration of call sites —
the model has to walk a hierarchy, read each candidate body, and tag it.

Full per-run data in [`graph-vs-nograph-data.csv`](graph-vs-nograph-data.csv).

## Setup

- **Model**: `gpt-5.4-mini` via the `openai` provider preset (both
  sides of every A/B).
- **Workspaces**: per-language upstream repos pinned by SHA — for
  example `JamesNK/Newtonsoft.Json @4f73e7`, `felangel/bloc @ff675f7`,
  `spf13/cobra @ad460ea`, `google/gson`, `akka/akka @d65463d`. Each
  scenario `.toml` records its repo and SHA under `[workspace.github]`.
- **No-graph variant**: the twelve graph tools (`repo_map`,
  `decl_search`, `definition_search`, `reference_search`,
  `symbol_context`, `hierarchy`, `read_slice`, `upstream_flow`,
  `downstream_flow`, `diff_context`, `plan_patch`,
  `refresh_compiler_facts`) are filtered out via the `excluded_tools`
  overlay and the graph-first exploration planner is disabled via
  `SQUEEZY_EXPLORATION_COMPILER=0`. Both halves of every A/B are
  otherwise identical.
- **System prompt**: a neutral instructions block ("answer concisely
  with whatever tools are available").

Scenarios live under
[`crates/squeezy-eval/fixtures/scenarios/graph-vs-nograph-{lang}-realworld-{with,no}-graph.toml`](../../../crates/squeezy-eval/fixtures/scenarios).

## Smoke results (n=1 per variant)

Cost is the total turn cost from `run.json` totals; tool_calls is the
count of `tool_call_completed` events in `trace.jsonl`. Recall is the
fraction of expected rows present in the final assistant answer,
hand-graded against the ground truth captured in each scenario's
`description` block.

| lang  | scenario                                      | variant     | tool_calls | cost     | recall   | trace |
|-------|-----------------------------------------------|-------------|-----------:|---------:|---------:|-------|
| csharp | Newtonsoft sync `Read*` override audit       | with_graph  | 23 | $0.0579 | 20/29 (69%)  | `target/eval/graph-vs-nograph-csharp-realworld-with-graph-1780218467813` |
| csharp | Newtonsoft sync `Read*` override audit       | no_graph    | 27 | $0.0724 | 29/29 (100%) | `target/eval/graph-vs-nograph-csharp-realworld-no-graph-1780218585042` |
| dart   | bloc_lint `_Listener` override classification | with_graph  | 12 | $0.0206 | 14/15 (93%)  | `target/eval/graph-vs-nograph-dart-realworld-with-graph-1780219490722` |
| dart   | bloc_lint `_Listener` override classification | no_graph    |  6 | $0.0192 | 14/15 (93%)  | `target/eval/graph-vs-nograph-dart-realworld-no-graph-1780219693878` |
| go     | spf13/cobra `*Command` setter scope audit    | with_graph  |  7 | $0.0257 | 17/17 (100%) | `target/eval/graph-vs-nograph-go-realworld-with-graph-1780218535992` |
| go     | spf13/cobra `*Command` setter scope audit    | no_graph    |  5 | $0.0175 | 17/17 (100%) | `target/eval/graph-vs-nograph-go-realworld-no-graph-1780219184410` |
| java   | gson `TypeAdapter` override audit            | with_graph  | 35 | $0.1020 | 18/18 (100%) | `target/eval/graph-vs-nograph-java-realworld-with-graph-1780222413962` |
| java   | gson `TypeAdapter` override audit            | no_graph    | 34 | $0.0747 | 18/18 (100%) | `target/eval/graph-vs-nograph-java-realworld-no-graph-1780222672822` |
| scala  | akka `RequiresMessageQueue` mailbox audit     | with_graph  | 30 | $0.0552 | 12/12 (100%) | `target/eval/smoke-scala-realworld/graph-vs-nograph-scala-realworld-with-graph-1780219137011` |
| scala  | akka `RequiresMessageQueue` mailbox audit     | no_graph    | 49 | $0.0911 | 12/12 (100%) | `target/eval/smoke-scala-realworld/graph-vs-nograph-scala-realworld-no-graph-1780219769654` |
| cpp    | spdlog `sink_it_` override audit             | both        | — | — | not yet run at n=1 | — |
| rust   | LlmProvider `name()` body classification     | both        | — | — | not yet run at n=1 | — |
| python | requests `Session` intra-class call graph    | both        | — | — | not yet run at n=1 | — |
| js     | lodash fp wrapper alias resolution           | both        | — | — | not yet run at n=1 | — |
| ruby   | sidekiq `Component` consumer audit           | both        | — | — | not yet run at n=1 | — |
| kotlin | detekt-rules-complexity Rule subclass audit  | both        | — | — | not yet run at n=1 | — |
| swift  | RoutesBuilder HTTP-method extension audit    | both        | — | — | not yet run at n=1 | — |
| php    | laravel Eloquent Relation concern audit      | both        | — | — | PHP scenario in flight | — |

## Headline finding

At n=1 on the six languages that smoke-ran successfully:

- **scala** is the cleanest win for the graph half: 30 vs 49 tool calls
  (-39%) and $0.0552 vs $0.0911 (-39%) at identical 12/12 recall. The
  no-graph half blew through `max_tool_calls_per_turn = 48` and still
  got the right answer, but at nearly double the cost.
- **dart** and **go** are roughly even on recall, with cost within
  noise. Dart with-graph spent 12 tool calls vs 6 for no-graph but came
  out only $0.0014 more expensive ($0.0206 vs $0.0192). Go with-graph
  was actually slightly more expensive ($0.0257 vs $0.0175) because
  `repo_map` ran on the cobra workspace as the first action.
- **csharp** is a regression at n=1: with-graph cost less ($0.0579 vs
  $0.0724) but missed the entire `TraceJsonReader` subclass (9 of the
  29 expected rows). The no-graph half hit 29/29. See "Bugs surfaced"
  below — the graph indexer reported `graph_available = false` for
  this Newtonsoft workspace, so the with-graph half effectively ran on
  grep + read_slice with the cost overhead of attempting graph tools
  first.
- **java** is a tie on recall (18/18 both sides) but the graph half
  loses on cost: $0.1020 vs $0.0747 (+36%) at 35 vs 34 tool calls. The
  with-graph half called `decl_search attribute=base:TypeAdapter
  path=gson/src/main/java` and got 27 packets back — correct on
  signal, but the response spilled to disk (~26 KB > inline budget)
  and the suffix-or-fuzzy `path` filter pulled in `gson/src/test/`
  classes the model still had to drop by hand. The model then issued
  23 grep calls and 6 read_slices to confirm `read(JsonReader)` /
  `write(JsonWriter, T)` overrides per candidate, which is exactly
  what the no-graph half did directly without the graph round-trip.
  This is structural for enumeration prompts where the answer needs
  a per-row body check: graph names the candidates but doesn't
  shortcut the body inspection grep already does cheaply.

Per-variant aggregates (sum across the five langs where both sides
completed — scala, dart, go, csharp, java):

| variant     | total tool_calls | total cost | wins (recall) |
|-------------|-----------------:|-----------:|---------------|
| with_graph  | 107 | $0.2614 | 4/5 ties or wins (scala, dart, go, java); csharp regression |
| no_graph    | 121 | $0.2749 | 5/5 ties or perfect (29/29 on csharp, 18/18 on java) |

Graph wins on aggregate cost (-5%) and matches or beats no-graph on
recall everywhere except csharp.

## Bugs surfaced

### 1. csharp `internal class` left out of subclass enumeration (with-graph)

`Src/Newtonsoft.Json/Serialization/TraceJsonReader.cs` declares
`internal class TraceJsonReader : JsonReader` and overrides all nine
sync `Read*` slots, all of them `delegate`-strategy
(`_innerReader.Read*()`). The with-graph run enumerated `BsonReader` (1
public), `JsonTextReader` (9), `JsonValidatingReader` (9), `JTokenReader`
(1) — but never opened `TraceJsonReader.cs`, missing 9 of 29 rows. The
no-graph run, running the same prompt, did find it (its grep sweep
over `public override.*Read` returned the `TraceJsonReader.cs`
matches alongside the others, and the model read each body).

Root cause is that this workspace's graph index came back unavailable
for the .NET tree, so the with-graph half fell back to grep — but
unlike the no-graph variant it spent its first 8 tool calls in
`definition_search` / `repo_map` /  `glob` exploration that returned
`graph_unavailable`, narrowing the remaining budget for the grep
sweep. The model never widened its grep pattern past `public override
.*Read` qualified by `JsonReader`-side filenames and so dropped the
one `internal class` member.

Cite: `target/eval/graph-vs-nograph-csharp-realworld-with-graph-1780218467813/run.json`
totals 23 tool calls / $0.0579 with the missing rows visible in the
final assistant_delta text;
`target/eval/graph-vs-nograph-csharp-realworld-no-graph-1780218585042/run.json`
totals 27 tool calls / $0.0724 and the final answer includes the nine
`TraceJsonReader.cs` rows.

Action: surface graph availability up-front in the agent preamble so
the model doesn't burn budget on graph tools that will return
`graph_unavailable`. Today the model only learns the graph is dead
after each individual tool call fails.

### 2. ruby nested-class fully-qualified path (Sidekiq::Scheduled::Enq)

The Ruby realworld scenario (`sidekiq/sidekiq` Component method-usage
audit) requires emitting fully-qualified Ruby class paths through
nested module declarations — for instance,
`Sidekiq::Scheduled::Enq` and `Sidekiq::Scheduled::Poller` both live
inside the same `module Sidekiq; module Scheduled; class Enq; ...`
file. Two of the fourteen expected rows are nested-class entries that a
naive grep-for-`include Sidekiq::Component` workflow returns under the
bare `Enq` / `Poller` names, dropping the `Sidekiq::Scheduled::`
prefix.

The Ruby smoke at n=1 has not yet been recorded (no
`target/eval/graph-vs-nograph-ruby-realworld-*` directories), so this
is currently sourced from the scenario design — the prompt explicitly
calls it out:

> Treat each `class Foo` ... `include Sidekiq::Component` ... `end`
> body that names the module on its own line as one entry. If two
> distinct classes inside the same file both `include
> Sidekiq::Component` (e.g. `Sidekiq::Scheduled::Enq` and
> `Sidekiq::Scheduled::Poller` in `lib/sidekiq/scheduled.rb`), report
> them as two separate rows.

Cite: `crates/squeezy-eval/fixtures/scenarios/graph-vs-nograph-ruby-realworld-with-graph.toml`
(prompt body, items "class" and the nested-class disambiguation rule).

Action: re-run the Ruby pair under `target/eval/` and grade the actual
recall. If the with-graph hierarchy walk drops the
`Sidekiq::Scheduled::` prefix on nested classes, file a graph bug
against the Ruby module-nest walker.

### 3. dart planner row truncation (one row dropped on both sides)

Both Dart smoke runs returned exactly 14 rows where the scenario
ground-truth has 15. The two outputs are otherwise identical:
`AvoidBuildContextExtensions` (3 rows), `AvoidFlutterImports` (1),
`AvoidPublicBlocMethods` (2), `AvoidPublicFields` (2), `PreferBloc`
(1), `PreferBuildContextExtensions` (1), `PreferCubit` (1),
`PreferFileNamingConventions` (1), `PreferVoidPublicCubitMethods` (2)
= 14. The missing row is the third
`PreferVoidPublicCubitMethods._Listener` override (the scenario
description specifies 15 total).

Because both variants drop the same row, the failure is upstream of
the graph/no-graph split — most likely the assistant emitted an
"and one more" thought during reasoning that never made it to a
visible output token, or stopped one row short of finishing. Worth
checking whether `model_output_bytes` hit a per-turn cap (the Dart
runs both ended at $0.02 — well under budget).

Cite: `target/eval/graph-vs-nograph-dart-realworld-with-graph-1780219490722`
and `target/eval/graph-vs-nograph-dart-realworld-no-graph-1780219693878`
— final assistant_delta concatenation, grep `packages/bloc_lint`
returns 14 in each.

Action: re-run dart at n=3 to confirm the truncation reproduces; if it
does, capture the missing row's reasoning trace and decide whether the
fix is prompt (explicit "emit all 15 rows" check) or planner (raise
the output-bytes cap on this scenario shape).

## Reproduce

```sh
# build the eval binary
cargo build -p squeezy-eval --release

# run a single realworld pair (csharp)
./target/release/squeezy-eval run \
  crates/squeezy-eval/fixtures/scenarios/graph-vs-nograph-csharp-realworld-with-graph.toml \
  --out target/eval --quiet
./target/release/squeezy-eval run \
  crates/squeezy-eval/fixtures/scenarios/graph-vs-nograph-csharp-realworld-no-graph.toml \
  --out target/eval --quiet

# all-language n=3 driver (writes to target/eval/realworld-n3-logs/<lang>.log)
target/eval/realworld-n3-logs/run-lang.sh csharp
```

Each run writes `run.json`, `trace.jsonl`, `frames.jsonl`,
`findings.jsonl`, and a `tickets/` directory; the per-run cost and
tool-call sequence used to build the table come from `run.json` +
`trace.jsonl`.

## Appendix: legacy small-scenario data

The original three-task A/B for this branch — single-symbol callers,
trait-implementor enumeration, cross-crate `estimate_cost` callers —
predates the realworld sweep and lives in this appendix for
back-reference. It exercised the graph fixes shipped on this branch
(self-crate qualified call, workspace-cross-crate qualified /
import-resolved reference) against the same `gpt-5.4-mini` model.

### Setup (legacy)

- **Workspace**: the squeezy repo itself (Rust workspace under
  `crates/`).
- **No-graph variant**: same `excluded_tools` overlay and
  `SQUEEZY_EXPLORATION_COMPILER=0` as the realworld sweep.

### Tasks (legacy)

| # | Prompt | Ground truth size | Why graph should help |
|---|---|---:|---|
| 1 | "List every call site of `run_scenario` (file, line, enclosing function)." | 3 sites (1 def + 2 callers) | Single-symbol reference traversal; classic `reference_search` use. |
| 2 | "List every Rust type in this repo that implements the `LlmProvider` trait." | 27 impls | Workspace-wide hierarchy; touches multiple crates. |
| 3 | "List every non-test call site of `squeezy_llm::estimate_cost`." | 6 callers | Cross-crate reference traversal with both qualified and bare-after-import call shapes. |

Ground truth verified by `rg`/`grep` against the working tree at HEAD
of this branch and re-verified after each fix.

### Headline numbers (legacy)

Medians across three runs per side per task, comparing the **current
state of the branch** (both fixes shipped) against the no-graph baseline.

| task | with-graph median $ | no-graph median $ | cost reduction | with-graph median recall | no-graph median recall |
|---|---:|---:|---:|---:|---:|
| 1 — `run_scenario` callers | $0.0225 | $0.0286 | **−21.3%** | 3/3 (100%) | 3/3 (100%) |
| 2 — `LlmProvider` impls | $0.0151 | $0.0604 | **−75.0%** | 18/27 (67%) | 27/27 (100%) |
| 3 — `estimate_cost` callers | $0.0359 | $0.0000* | n/a* | 6/6 (100%) | 0/6* |

\* Task 3 median is degenerate for the no-graph side: two of three
no-graph runs gave up returning empty answers ($0.0000); the one run
that finished cost $0.0888 — more than 2× the graph median. See the
per-run table.

**Tool-call medians** (graph vs no-graph): task 1 12 vs 14, task 2 4 vs
32, task 3 17 vs 1. Note that no-graph's task-2 median (32 tool calls)
delivers 27/27 recall while graph's median (4 tool calls) sometimes
stops short — see "Where graph wins" below.

### Per-run data (legacy)

#### Task 1 — `run_scenario` callers

| variant | run | tool calls | events | cost | recall |
|---|---|---:|---:|---:|---:|
| with-graph (post both fixes) | 1 | 12 | 723 | $0.0225 | 3/3 |
| with-graph (post both fixes) | 2 | 9 | 227 | $0.0201 | 3/3 |
| with-graph (post both fixes) | 3 | 16 | 937 | $0.0314 | 3/3 |
| no-graph | 1 | 15 | 616 | $0.0247 | 3/3 |
| no-graph | 2 | 14 | 364 | $0.0286 | 3/3 |
| no-graph | 3 | 13 | 519 | $0.0326 | 3/3 |

**Pre-fix reference point** (single run, before the self-crate
fallback): 6 tool calls, 110 events, $0.0144, **1/3** recall — graph
returned only the `ci.rs` references and silently missed
`main.rs:172`. Documented in
`references_to_symbol_finds_qualified_self_crate_call_across_modules`.

#### Task 2 — `LlmProvider` impls (27 ground truth)

| variant | fix level | run | tool calls | events | cost | recall |
|---|---|---|---:|---:|---:|---:|
| with-graph | pre-fix | 1 | 7 | 322 | $0.0192 | 16/27 |
| with-graph | pre-fix | 2 | 5 | 344 | $0.0239 | 18/27 |
| with-graph | pre-fix | 3 | 32 | 2228 | $0.0578 | 18/27 |
| with-graph | post both fixes | 1 | 4 | 304 | $0.0151 | 20/27 |
| with-graph | post both fixes | 2 | 4 | 386 | $0.0082 | 7/27 |
| with-graph | post both fixes | 3 | 32 | 1154 | $0.0662 | 18/27 |
| no-graph | baseline | 1 | 32 | 1881 | $0.0685 | 27/27 |
| no-graph | baseline | 2 | 3 | 758 | $0.0130 | 27/27 |
| no-graph | baseline | 3 | 32 | 1049 | $0.0604 | 27/27 |

The post-fix graph ceiling is higher (one earlier run reached 25/27)
than pre-fix (max 18/27), but the median doesn't move because on
"list every X in the workspace" prompts the model frequently picks
`grep` as its first move even when graph tools are available. See
"Where graph still falls short."

#### Task 3 — `estimate_cost` callers (6 ground truth)

| variant | fix level | run | tool calls | events | cost | recall |
|---|---|---|---:|---:|---:|---:|
| with-graph | pre-fix | 1 | 28 | 643 | $0.0883 | 4/6 |
| with-graph | pre-fix | 2 | 6 | 481 | $0.0288 | 1/6 |
| with-graph | pre-fix | 3 | 11 | 307 | $0.0000 | 0/6 |
| with-graph | post both fixes | 1 | 17 | 609 | $0.0359 | 6/6 |
| with-graph | post both fixes | 2 | 27 | 573 | $0.0547 | 6/6 |
| with-graph | post both fixes | 3 | 12 | 514 | $0.0213 | 1/6 |
| no-graph | baseline | 1 | 25 | 798 | $0.0888 | 6/6 |
| no-graph | baseline | 2 | 1 | 113 | $0.0000 | 0/6 |
| no-graph | baseline | 3 | 1 | 10 | $0.0000 | 0/6 |

Two no-graph runs delegated to the `explore` subagent and gave up
without an answer; the one no-graph run that finished cost more than
twice any post-fix graph run with the same final recall. Pre-fix
graph runs were unreliable too (one returned correct count with wrong
caller names, one returned 1, one returned nothing); post-fix two of
three runs are perfect.

### Where graph wins the most (legacy)

**Cross-crate single-symbol traversal under a tight budget** — task 3
is the cleanest case for the graph value prop. The graph-resolved
`reference_search` returns the seven workspace call sites in one
call; the model then `read_slice`s each to extract caller names. No-
graph has to fan out across crates with `grep -r` and chase line
numbers manually; in two of three runs the model abandoned the task
before reporting an answer. Where the no-graph run did finish, it
cost 2.5× the post-fix graph median.

**Same-symbol, few sites, mixed call shapes** — task 1 is the
incremental case. Both halves can solve it; the graph half does so at
~21% lower cost and ~40% fewer tool calls. The pre-fix graph half
silently missed the qualified `squeezy_eval::run_scenario` call from
`main.rs`; the self-crate qualified-callable fallback now surfaces
that hit, so the graph side now matches no-graph on recall while
keeping its cost edge.

### Where graph still falls short (legacy)

**Workspace-wide structural enumeration (task 2)** — when the prompt
is "list every implementor / definition / type matching X", the
model often picks `grep` first regardless of whether graph tools are
advertised. Post-fix the graph CAN find every impl (one earlier run
hit 25/27), but on the cleaner three-run median the model's choice
of `grep` as first step caps recall at 18/27. The remaining gap is a
prompt/planner question, not a graph capability question. A
follow-up nudge in `squeezy-agent`'s default instructions — "for
'list every implementor / definition / type' questions, prefer
`decl_search` over `grep`" — should close it.

### Fixes shipped (legacy)

Two binding-rule additions in `squeezy-graph`. Each is gated by a
unique-workspace-candidate-by-name check, so ambiguous names stay
unresolved.

#### 1. Self-crate qualified call

`<mycrate>::foo()` from another module of the same crate now resolves
to the function in that crate. Tree-sitter emits the `Calls` edge
with `to = None` because `module_qualified_call` does not treat the
crate's own underscore name as an alias for the crate root, and the
binding chain falls through into rules that reject `Function` symbols
on `reference_kind_can_bind_symbol`. The new
`self_crate_qualified_callable_matches` runs before the call-edge and
semantic-edge branches and binds with `Heuristic` confidence when the
symbol is the unique same-crate callable of its name.

Unit tests:

- `references_to_symbol_finds_qualified_self_crate_call_across_modules`
- `self_crate_qualified_callable_does_not_bind_when_name_is_ambiguous_in_crate`

#### 2. Workspace-cross-crate qualified or import-resolved reference

`<othercrate>::Foo` from a different workspace crate, and bare `foo()`
after `use othercrate::foo;`, now resolve to the symbol in
`crates/othercrate/`. The default `reference_is_in_symbol_package`
gate rejected cross-crate references before the binding chain could
look at the qualified path or the file's imports. The new
`workspace_cross_crate_qualified_match` runs before that gate and
recovers the qualifier from one of three sources: the reference text
itself, the source-byte scope prefix adjacent to a bare-leaf
reference, or a non-glob `use <crate>::Name [as alias]` import in the
reference's file.

Unit tests:

- `references_to_symbol_finds_workspace_cross_crate_qualified_trait_impl`
- `references_to_symbol_finds_workspace_cross_crate_bare_call_after_use_import`
- `workspace_cross_crate_qualified_match_does_not_bind_ambiguous_workspace_name`
- `graph_symbol_references_surface_qualified_workspace_cross_crate_uses`
  (rewritten from
  `graph_symbol_references_are_package_local_until_cargo_resolution_exists`,
  which documented the pre-resolution behaviour we are now partially
  retiring)

## Appendix: Architectural audits (Codex baseline)

Seven architectural-audit scenarios — "list every X that derives from /
implements / imports / writes Y" — run three times each against Codex
CLI (`codex exec` with `gpt-5.4-mini`, ephemeral, JSON output, no
graph) as an external baseline for the same scenarios squeezy
exercises with and without its semantic-graph tool family. Codex
artifacts live under `/tmp/codex-runs/architectural/{lang}-r{1,2,3}.{events.jsonl,answer.txt}`;
metrics are appended to
[`graph-vs-nograph-data.csv`](graph-vs-nograph-data.csv) as
`{lang}_architectural` rows with variant `codex_baseline`. Costs are
medians of three runs; squeezy figures come from the same scenarios
under `crates/squeezy-eval/fixtures/scenarios/graph-vs-nograph-{lang}-architectural-{with,no}-graph.toml`
(Go was not in the squeezy validation sweep).

| scenario | codex $ | squeezy with-graph $ | squeezy no-graph $ | codex recall | cost winner |
|---|---:|---:|---:|---:|:--|
| Rust — `ToolResult` struct literals (4) | $0.0359 | $0.0276 | $0.0443 | 4/4 (100%) | **squeezy with-graph** |
| Go — `ValidArgsFunction` writes (6) | $0.0208 | — | — | 6/6 (100%) | **codex** |
| C++ — `base_sink<Mutex>` direct subclasses (21) | $0.0541 | $0.0189 | $0.0219 | 20/21 (95%) | **squeezy with-graph** |
| C# — `JsonReader` subclasses (5) | $0.0278 | $0.0270 | $0.0096 | 5/5 (100%) | **squeezy no-graph** |
| Java — `TypeAdapter` subclasses (11) | $0.0497 | $0.0244 | $0.0409 | 11/11 (100%) | **squeezy with-graph** |
| JS — lodash importer-helper pairs (16) | $0.0176 | $0.0236 | $0.0172 | 16/16 (100%) | **squeezy no-graph** |
| Python — `RequestException` subclasses + raises (21) | $0.0365 | $0.0261 | $0.0310 | 21/21 (100%) | **squeezy with-graph** |

**Headline**: squeezy with-graph beats codex on cost in 4 of 6
comparable scenarios (rust, cpp, java, python), squeezy no-graph beats
codex in the other 2 (csharp, js), and codex never wins on cost
against squeezy. Codex recall is essentially perfect across the board
(only one miss: `dist_sink` on the cpp prompt, lost to a literal
reading of the example exclusion list). The lone scenario where codex
beats no head-to-head squeezy reference is Go — squeezy did not run
that scenario in the validation sweep.

The cost gap is larger than the cost gap on the callers/refactor
sweeps: codex spends 2-3× squeezy with-graph on Rust, C++, Java, and
Python — primarily because codex repeatedly re-reads large files via
`sed -n` rather than using a graph-resolved slice. Squeezy with-graph
matches or beats codex recall everywhere except cpp (one run hit
18/21) and java (two runs short of 11/11), where the same
"`grep`-first" caveat noted under "Where graph still falls short"
applies — but the median post-fix squeezy with-graph cost is still
below codex on every comparable scenario.
