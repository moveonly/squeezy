# Cheap-Model Fast Path (Per-Turn Routing)

## Motivation

Most user turns on a coding agent do not need the headline model. A
session running on Opus or GPT-5.5 pays the headline rate every turn,
but a sizable share of those turns are well-specified mechanical
asks — "run `cargo test -p squeezy-llm`", "checkout main", "rename
`frobnicate` to `process` in `src/lib.rs`", "grep TODO under `src/`".
The same provider's small-fast tier — Anthropic Haiku, OpenAI Nano /
Mini, Gemini Flash Lite, the cheap Bedrock variant — finishes those
turns correctly at roughly 1/15 the input price. The savings compound
across an entire session: a 30-turn coding session where ~30% of turns
are slam-dunk operational asks routes those turns to the cheap tier,
trims the headline-rate bill on the rest, and never blocks on a
provider switch because the swap stays within the configured
provider.

Squeezy's earlier subagent infrastructure already routed Explorer and
Reviewer subagents to the cheap tier via `RoleModelPolicy::Cheap`
(`crates/squeezy-agent/src/roles.rs:32-52`) and
`small_fast_model_for_provider` (`crates/squeezy-core/src/lib.rs:64`),
but the main user-facing turn always used `AppConfig.model`. This
chapter covers the layer that extends cheap-tier routing to that
headline turn — with a robust mid-turn fallback for false positives.

## Mechanism

### Two-layer classifier in `turn_router.rs`

The router lives in `crates/squeezy-agent/src/turn_router.rs`. Each
turn's first action (after the `PreTurn` hook and skill activation) is
to call `classify_turn`, which returns a
`TurnRoutingDecision::{ Cheap { reason, model } | Parent }`. The
decision is consumed once and threaded through every round of the
turn loop as `current_model: Arc<str>` — the same handle that becomes
`LlmRequest::model` at the dispatch site
(`crates/squeezy-agent/src/lib.rs:5288`).

Layer 1 is a **strict heuristic prefilter**. It admits a prompt only
when *all* of the following hold:

- single sentence (`HEURISTIC_MAX_SENTENCES = 1`),
- ≤ 15 words (`HEURISTIC_MAX_WORDS`),
- ≤ `heuristic_max_chars` characters (default 2_000 ≈ 400 tokens),
- no multi-paragraph break (`\n\n`),
- no ambiguity marker (`maybe`, `figure out`, `decide`, `investigate`,
  `legacy`, `across the`, `any test`, `any file`, …),
- no compound connector (`, then`, `and check`, `and verify`,
  `and ensure`, `and make sure`, …),
- first non-filler word in a tight imperative whitelist
  (`run`, `ls`, `cat`, `grep`, `checkout`, `rename`, `format`, `fmt`,
  `lint`, `fetch`, `stash`, `tag`).

The whitelist is deliberately narrow — `delete`, `move`, `add`,
`remove`, `refactor` were considered and excluded because they
compound too often with workspace-level reasoning ("delete the
**legacy** auth module", "add a new field to AppConfig"). Anything the
heuristic does not accept falls through to Layer 2.

Layer 2 is the **provider-cheap-tier LLM judge**. It runs only when:

- `[routing].llm_judge = true` (default true),
- prompt is non-empty and within `judge_max_chars` (default 6_000 ≈
  1500 tokens),
- there is no image attachment and no `/parent` override,
- the escalation-sticky window is not engaged.

The judge dispatches a single short request via `LlmProvider::stream_response`
to **the same cheap-tier model that would handle the routed turn** —
resolved through `cheap_model_for(provider, config)`
(`crates/squeezy-agent/src/lib.rs:8630-8641`), shared with the
subagent path. That means the judge model varies by provider:

| Parent provider                | Judge / routed-turn model           |
|--------------------------------|--------------------------------------|
| Anthropic (Opus, Sonnet)       | Claude Haiku                         |
| OpenAI (GPT-5, GPT-5.5)        | GPT-mini / Nano                      |
| Google (Gemini Pro)            | Gemini Flash Lite                    |
| Bedrock                        | Cheap variant from `models.json`     |
| OpenRouter / Vercel / Portkey  | The route's small-fast tier           |
| Ollama / local                 | User's `[model].small_fast_model`    |

The judge's system prompt is fixed (`turn_router.rs::JUDGE_INSTRUCTIONS`)
and asks for a strict JSON reply `{"route":"cheap"|"parent","reason":"…"}`
with `max_output_tokens = 80` and `reasoning_effort = Low`. The
classifier parses the JSON, optionally stripping a `\`\`\`json` fence,
and treats any timeout (10 s), parse error, or non-`cheap`/`parent`
verdict as `Parent`. The judge **never** blocks a turn.

### Dispatch hook + mid-turn escalation

The classifier runs once per turn, just before the round loop opens
in `Agent::run` (`crates/squeezy-agent/src/lib.rs` at the top of the
`for round in 0..MAX_TOOL_ROUNDS` loop). The decision sets
`current_model` and an `on_cheap_turn: bool` flag the loop carries
across rounds.

At the top of every round (before assembling the next `LlmRequest`),
the loop calls `EscalationState::maybe_trigger` if `on_cheap_turn` is
still true. The detector fires on any of:

1. tool-call count for the turn exceeds
   `routing.resolved_cheap_escalation_tool_calls(max_tool_calls_per_turn)`
   — the default derives as `max_tool_calls_per_turn / 4` so the
   ceiling tracks the user's existing budget choices instead of
   carrying its own magic number,
2. `tool_errors + budget_denials ≥ routing.cheap_escalation_error_threshold`
   (default `2`),
3. the buffered assistant text matches any low-confidence phrase
   ("i'm not sure", "this is complex", "need more context", "i can't",
   "i cannot", "unable to", "let me think").

When the detector fires, the loop:

- swaps `current_model = parent_model` so the upcoming round and every
  subsequent round dispatches on the headline model,
- engages the **escalation-sticky window**: the next
  `routing.escalation_sticky_turns` user prompts skip the router and
  go straight to the parent model, so a follow-up clarification turn
  in the middle of a hard task does not flap back to cheap,
- emits an `AgentEvent::TurnRouted { from, to, reason: "escalated_<signal>" }`
  event so transcripts, eval frames, and the TUI show the swap with
  a structured reason string.

Because the swap is within-provider (`small_fast_model_for_provider`
returns `None` and the decision degrades to `Parent` for providers
without a curated cheap tier), the conversation state is intact
across the swap — no replay, no `previous_response_id` reset.

### Per-turn user overrides (`/cheap`, `/parent`, `/router`)

Agent exposes three public methods backing the matching slash
commands (the TUI/CLI wires them in `crates/squeezy-cli`):

- `Agent::request_routing_force_cheap()` — one-shot, forces cheap on
  the next turn even when the heuristic would not have fired and the
  judge would have voted parent.
- `Agent::request_routing_force_parent()` — one-shot, bypasses the
  router entirely for the next turn.
- `Agent::set_routing_session_disabled(bool)` — session-wide toggle.
  When `true`, the router never picks cheap implicitly; explicit
  `/cheap` overrides still work.

The overrides live on a shared `Arc<StdMutex<RoutingPersistentState>>`
field on `Agent`. `force_cheap` / `force_parent` are consumed at the
top of `start_turn`; `session_disabled` persists.

### Telemetry surfaces

`TurnMetrics` (`crates/squeezy-core/src/lib.rs`) carries:

- `routing_judge_usd_micros` — provider-reported cost of the judge
  call when one ran.
- `routed_to_cheap: bool` — first round dispatched on the cheap tier.
- `escalated_to_parent: bool` — escalation detector handed back to
  parent mid-turn.
- `routing_estimated_savings_usd_micros` — savings vs. running the
  same turn on the parent model (post-hoc re-price of the same token
  counts at parent rates).

`SessionMetrics` mirrors these as cumulative session counts. The
`/context` panel and `CostUpdate` events surface them so the user can
audit how much the router actually saved.

## Cost intuition

Headline rate ratios (provider docs, snapshot at branch creation
2026-05-31):

| Provider        | Parent (per Mtok in)    | Cheap (per Mtok in)    | Cheap multiplier |
|-----------------|-------------------------|-------------------------|------------------|
| Anthropic Opus 4.7         | $15.00 | Haiku 4.5: $1.00     | ~15× |
| OpenAI GPT-5.5             | $2.50  | GPT-5.4 Nano: $0.15  | ~17× |
| Google Gemini 2.5 Pro      | $1.25  | Flash Lite: $0.10    | ~13× |

A turn the heuristic routes to cheap costs ~1/13–1/17 of the
parent-rate equivalent for the same wire bytes. With the LLM judge
enabled, a borderline classification adds one short judge call
(~120 input tokens + ~30 output) — the judge cost on Anthropic is
~$0.0005 per turn it runs. The break-even point is roughly: route a
prompt to cheap when expected parent cost ≥ 1.4× judge cost, which
holds for essentially any non-trivial parent turn.

## Edge cases & limits

- **Provider has no cheap tier**: `small_fast_model_for_provider`
  returns `None` (e.g. user-provided Ollama or an unfamiliar
  OpenAI-compatible preset). The decision degrades to `Parent` and
  the router logs a one-shot info note for the session. Covered by
  the path test `cheap_model_for_returns_none_falls_back_to_parent`.
- **Cheap == parent**: when the configured `[model].small_fast_model`
  override resolves to the same id as `AppConfig.model`, the router
  short-circuits to `Parent` without consulting the judge — routing
  would be a no-op.
- **Image input**: when `routing.bypass_for_images = true` (default),
  any turn carrying an image attachment routes to the parent model.
  Cheap tiers are typically weaker on vision and the user already
  paid for visual input.
- **Cancellation**: the judge call rides the same `CancellationToken`
  as the parent turn. A user cancellation while the judge is mid-flight
  drops the judge and the turn dispatches on the parent model.
- **Heuristic-bypass paths**: `/parent` forces parent; `enabled = false`
  forces parent (still honors explicit `/cheap`); the escalation-sticky
  window forces parent for K turns after a recent escalation.
- **Within-provider only**: today the cheap tier is always the same
  *provider*'s small-fast tier. Cross-provider routing (e.g. Opus
  parent → Gemini Flash Lite cheap) is not in scope — would need
  duplicated auth / token-accounting and would change the
  prompt-cache prefix.
- **Subagents already routed**: the Explorer/Reviewer subagents
  separately go through `subagent_model_for_kind` with
  `RoleModelPolicy::Cheap`. This chapter's mechanism is orthogonal —
  the cheap-tier resolver function `cheap_model_for` is shared by
  both paths so an override in `[model].small_fast_model` applies
  uniformly.

## Configuration

```toml
[routing]
enabled = true                     # opt-out master switch
llm_judge = true           # borderline judge call
cheap_escalation_tool_calls = 0       # 0 = max_tool_calls_per_turn / 4
cheap_escalation_error_threshold = 2
escalation_sticky_turns = 3
bypass_for_images = true
heuristic_max_chars = 2000
judge_max_chars = 6000
```

Each key has an `SQUEEZY_ROUTING_*` env-var equivalent matching the
existing per-budget convention (e.g. `SQUEEZY_ROUTING_ENABLED=0`).

## Reliability: heuristic vs. judge-only

The heuristic is deliberately narrow because false positives bypass
the judge's second-opinion call entirely. The
`heuristic_rejects_every_adversarial_false_positive` unit test in
`turn_router_tests.rs` codifies a corpus of prompts that *look*
simple but actually carry hidden complexity — compound asks,
ambiguous scopes (`legacy`, `any test`), multi-sentence asks,
reasoning verbs. Every adversarial prompt must defer to the judge or
to `Parent`; if a future widening of the whitelist regresses that
test, the change has to be paired with a tightened bypass or moved
into the judge layer.
