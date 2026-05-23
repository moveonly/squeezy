# Validation Harness

The validation harness measures Squeezy behavior against small, repeatable coding tasks. It is intentionally deterministic by default so PRs can run it without network access or model spend.

## Task Specs

Tasks are TOML files with:

- `id`, `title`, and `prompt`
- `workspace.files` inline fixture files
- `expect.contains` required substrings in the final answer
- `mock.openai.events` and `mock.anthropic.events` normalized provider traces
- `baseline` grep/read hints for the deterministic baseline runner

Mock traces use the same event shape that costly live runs can emit: `started`, `text_delta`, and `completed` events with optional token counts.

## Local Runs

Run deterministic tasks:

```sh
cargo run -p squeezy-harness -- run --jsonl target/harness.jsonl
```

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

The default Anthropic costly model is `claude-3-5-haiku-20241022`. Override costly models with `SQUEEZY_COSTLY_OPENAI_MODEL`, `SQUEEZY_COSTLY_ANTHROPIC_MODEL`, or the shared `SQUEEZY_COSTLY_MODEL`.

## CI

CI runs only deterministic harness modes:

- `mock-openai`
- `mock-anthropic`
- `grep-baseline`

Costly runners are never enabled by default and require `SQUEEZY_RUN_COSTLY_TESTS=1` plus the provider API key.
