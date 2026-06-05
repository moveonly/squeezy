# Validation Harness

The validation harness measures Squeezy behavior against small, repeatable coding tasks. It is intentionally deterministic by default so PRs can run it without network access or model spend.

> For agent-driven exploratory QA against a real workspace, see [`EVAL_HARNESS.md`](./EVAL_HARNESS.md) (`squeezy-eval`). The two harnesses are complementary: this one is for deterministic CI fixtures; eval is for live-agent runs that produce trace + ticket artifacts.

## Task Specs

Tasks are TOML files with:

- `id`, `title`, and `prompt`
- `workspace.files` inline fixture files
- `expect.contains` required substrings in the final answer
- `mock.openai.events` and `mock.anthropic.events` normalized provider traces
- `replay.trace` path to a redacted session replay tape for replay regression
  runs
- `baseline` grep/read hints for the deterministic baseline runner

Mock traces use the same event shape that costly live runs can emit: `started`, `text_delta`, and `completed` events with optional token counts.

## Local Runs

Run deterministic tasks:

```sh
cargo run -p squeezy-harness -- run --jsonl target/harness.jsonl
```

Compare exploration-compiler behavior without live provider spend:

```sh
cargo run -p squeezy-harness -- run \
  --runner planner-probe \
  --runner planner-probe-no-planner \
  --jsonl target/planner-probe.jsonl
```

`planner-probe` uses the normal agent with `[agent].exploration_compiler=true`.
`planner-probe-no-planner` disables that setting for the same task fixtures.
Both runners report `planner_turns`, `planner_tool_calls`,
`planner_refusals`, and the usual read/tool metrics in JSONL output.

Replay a recorded session tape:

```sh
cargo run -p squeezy-harness -- run --runner replay --jsonl target/replay.jsonl
```

Replay tasks declare a `[replay]` table with a `trace` path relative to the
tasks directory, plus optional `provider`, `model`, and `mode`. The replay
runner uses the recorded model stream and tool results without provider keys or
live tool effects, and fails when recorded request/tool hashes diverge.

List bundled tasks:

```sh
cargo run -p squeezy-harness -- list
```

Run a live OpenAI smoke pass:

```sh
SQUEEZY_RUN_COSTLY_TESTS=1 \
OPENAI_API_KEY=... \
cargo run -p squeezy-harness -- run --runner costly-openai --trace-dir target/harness-traces
```

Run a live Anthropic Haiku smoke pass:

```sh
SQUEEZY_RUN_COSTLY_TESTS=1 \
ANTHROPIC_API_KEY=... \
cargo run -p squeezy-harness -- run --runner costly-anthropic --trace-dir target/harness-traces
```

Costly runners use the provider defaults from `squeezy-core`. Override costly
models with `SQUEEZY_COSTLY_OPENAI_MODEL`, `SQUEEZY_COSTLY_ANTHROPIC_MODEL`,
provider-specific variables such as `SQUEEZY_COSTLY_GOOGLE_MODEL`, or the
shared `SQUEEZY_COSTLY_MODEL`. `SQUEEZY_COSTLY_MAX_OUTPUT_TOKENS` can set a
positive per-run output cap; unset means the normal default cap is used.

Additional explicit live runners are available for provider smoke testing:
`costly-google`, `costly-azure-openai`, `costly-ollama`, and
`costly-bedrock`. They use the same provider configuration described in
[`../../crates/squeezy-skills/external-docs/PROVIDERS.md`](../../crates/squeezy-skills/external-docs/PROVIDERS.md).

## CI

CI runs only deterministic harness modes:

- `mock-openai`
- `mock-anthropic`
- `planner-probe`
- `planner-probe-no-planner`
- `grep-baseline`

Costly runners are never enabled by default and require `SQUEEZY_RUN_COSTLY_TESTS=1` plus the provider API key.
