# Provider And Model Configuration

The full configuration model, source precedence, templates, and inspection
commands are documented in [`CONFIGURATION.md`](CONFIGURATION.md). This page
focuses on provider-specific fields.

Squeezy resolves provider settings from the same chain as the rest of the
configuration system. From highest to lowest precedence:

1. CLI flags
2. Environment variables
3. Project `squeezy.toml` (nearest ancestor)
4. User `~/.squeezy/settings.toml`
5. Built-in defaults

See [`CONFIGURATION.md`](CONFIGURATION.md) for the merging rules and the
`config inspect` / `doctor` source reporting. The default user settings
path can be overridden with `SQUEEZY_SETTINGS_PATH`.

## Choose your provider

Squeezy ships 18 provider presets across four buckets. Use the decision
tree below; per-provider sections follow.

- **Don't have any vendor account?** Start with **OpenRouter**. One credit
  account routes to every frontier model under a single key.
- **Want frontier-quality on a single vendor bill?** Use the first-party
  preset: `openai`, `anthropic`, or `google`.
- **Already on a cloud platform?** Use the platform-IAM preset:
  `bedrock` (AWS), `azure_openai` (Azure), or `vertex` (GCP).
- **Want maximum tokens-per-second on open-weight models?** Try `groq` or
  `cerebras` (Llama 3.x at 500-1800 tok/s).
- **Want the cheapest open-weight access?** Try `deepseek` (its own
  DeepSeek-V3 / R1) or `together` / `fireworks` / `deepinfra` (Llama,
  Qwen, Mixtral).
- **Running models locally?** Use `ollama` for Ollama; the
  `openai_compatible` custom preset for LM Studio, vLLM, or llama.cpp
  server.
- **Need an org-wide proxy with rate limits and routing?** Use
  `vercel` (Vercel AI Gateway), `portkey` (PortKey), or point the
  `openai_compatible` preset at a self-hosted LiteLLM / Cloudflare
  AI Gateway endpoint.

Recommended models below are seed defaults; the full catalog for each
provider lives at <https://models.dev/>.

## Quick start: OpenRouter

```toml
[model]
provider = "openrouter"

[providers.openrouter]
api_key_env = "OPENROUTER_API_KEY"
default_model = "anthropic/claude-opus-4-7"
```

Then export the key and run `squeezy`:

```sh
export OPENROUTER_API_KEY=...   # https://openrouter.ai/keys
squeezy
```

## Per-provider reference

`model = ""` (or omitted) means Squeezy uses the provider's
`default_model`. `profile` is a routing/telemetry tag; accepted values
are `cheap`, `balanced`, and `strong`. Every section accepts the shared
fields `api_key_env`, `api_key`, `base_url`, `default_model`, and
`extra_headers`; only fields that differ from the defaults are shown
below.

### Aggregators (one credit, many models)

#### `openrouter` — OpenRouter

- Env: `OPENROUTER_API_KEY`. Base URL: `https://openrouter.ai/api/v1`.
- Default model: `anthropic/claude-opus-4-7`. Catalog: 100+ models from
  30+ underlying providers (Anthropic, OpenAI, Mistral, Llama family,
  DeepSeek V3/R1, Qwen, Gemma, command-r-plus, Yi, etc).
- Forwards `prompt_cache_key` so Squeezy's cache-aware billing keeps
  working when routing through OpenRouter.
- Ships default `HTTP-Referer` / `X-Title` headers for traffic
  attribution; override or unset under `[providers.openrouter.headers]`.

```toml
[providers.openrouter]
api_key_env = "OPENROUTER_API_KEY"
default_model = "anthropic/claude-opus-4-7"

[providers.openrouter.headers]
"HTTP-Referer" = "https://github.com/esqueezy/squeezy"
"X-Title" = "Squeezy"
```

#### `vercel` — Vercel AI Gateway

- Env: `AI_GATEWAY_API_KEY`. Base URL:
  `https://ai-gateway.vercel.sh/v1`.
- Default model: `anthropic/claude-opus-4-7`.
- Same OpenAI-compatible wire as OpenRouter; pricing and per-model
  capability come from Vercel's gateway catalog.

```toml
[providers.vercel]
api_key_env = "AI_GATEWAY_API_KEY"
default_model = "anthropic/claude-opus-4-7"
```

#### `portkey` — PortKey

- Env: `PORTKEY_API_KEY`. Base URL: `https://api.portkey.ai/v1`.
- PortKey is a gateway, not a model host; it routes to an upstream
  configured at request time. Four routing modes:
  1. **Integration-prefixed model id** (most common): `model =
     "@openrouter/qwen/qwen3.6-35b-a3b"`. Hit `GET
     https://api.portkey.ai/v1/models` with your key to enumerate every
     available id.
  2. **Virtual Key**: set the upstream credential PortKey-side, then send
     `x-portkey-virtual-key` and use the upstream's native model id.
  3. **Config**: define routes and aliases PortKey-side, then send
     `x-portkey-config` and use the alias as the model. If the Config is
     attached to your User key, the header is optional.
  4. **Direct provider header**: set `x-portkey-provider` (rare).
- Squeezy passes the model id and any `[providers.portkey.headers]`
  through verbatim.

```toml
[providers.portkey]
api_key_env = "PORTKEY_API_KEY"
# api_key = "..."   # or `squeezy auth set portkey`

[providers.portkey.headers]
"x-portkey-virtual-key" = "vk-xxxxxxxxxxxx"
```

### First-party vendor APIs

#### `openai` — OpenAI

- Env: `OPENAI_API_KEY`. Base URL: `https://api.openai.com/v1`.
- Default model: `gpt-5.5`. Uses the OpenAI Responses wire (streaming,
  function tools, cached-token usage, response state).

```toml
[providers.openai]
api_key_env = "OPENAI_API_KEY"
default_model = "gpt-5.5"
```

#### `anthropic` — Anthropic

- Env: `ANTHROPIC_API_KEY`. Base URL: `https://api.anthropic.com/v1`.
- Default model: `claude-opus-4-7`. Uses the Anthropic Messages wire
  (streaming, function tools, cache read/write usage).

```toml
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
default_model = "claude-opus-4-7"
```

#### `google` — Google Gemini

- Env: `GEMINI_API_KEY`. Base URL:
  `https://generativelanguage.googleapis.com/v1beta`.
- Default model: `gemini-2.5-pro`. Uses Gemini `streamGenerateContent`
  SSE streaming with function declarations, function calls, and usage
  metadata.

```toml
[providers.google]
api_key_env = "GEMINI_API_KEY"
default_model = "gemini-2.5-pro"
```

### Cloud-platform hosts

Models hosted on a cloud platform's infrastructure behind that
platform's IAM rather than a vendor API key.

#### `bedrock` — Amazon Bedrock

- AWS multi-vendor catalog. Uses the AWS default credential chain (no
  `api_key_env`). Default model:
  `anthropic.claude-haiku-4-5-20251001-v1:0`. Transport is the AWS SDK
  Bedrock Runtime `ConverseStream`.

```toml
[providers.bedrock]
region = "us-east-1"
default_model = "anthropic.claude-haiku-4-5-20251001-v1:0"
```

#### `azure_openai` — Azure OpenAI Service

- The OpenAI-only slice of Azure's model catalog. Env:
  `AZURE_OPENAI_API_KEY`. Default model: `gpt-5.5`. Uses Responses-
  compatible streaming with `api-key` auth and `api-version`. For the
  broader Foundry catalog (multi-vendor Azure AI Studio deployments),
  use the `openai_compatible` preset further down.

```toml
[providers.azure_openai]
api_key_env = "AZURE_OPENAI_API_KEY"
base_url = "https://RESOURCE.openai.azure.com/openai/v1"
api_version = "v1"
default_model = "gpt-5.5"
```

#### `vertex` — Google Vertex AI

- Gemini and partner models via OAuth2 access tokens. Either provide a
  short-lived token via `VERTEX_ACCESS_TOKEN`, or point at a
  service-account JSON; Squeezy refreshes the token transparently in
  the latter case. The base URL is templated from
  `vertex_project` + `vertex_location`.

```toml
[providers.vertex]
api_key_env = "VERTEX_ACCESS_TOKEN"
# service_account_json = "/path/to/key.json"
vertex_project = "my-gcp-project"
vertex_location = "us-central1"
default_model = "google/gemini-2.5-pro"
```

### Local runtime

#### `ollama` — Ollama

- Base URL: `http://localhost:11434/api`. Default model: `qwen3-coder`.
- Uses `/api/chat` NDJSON streaming with function tool schemas and
  zero-dollar pricing. An opt-in `/v1` route is available for
  OpenAI-compatible Ollama wire; see [`CONFIGURATION.md`](CONFIGURATION.md).
- Model metadata (context window) is queried from `/api/show`; if
  absent, the context window is reported as unknown.

```toml
[providers.ollama]
base_url = "http://localhost:11434/api"
default_model = "qwen3-coder"
```

### Single-vendor OpenAI-compatible (full preset)

These three providers have curated model rows in the registry — context
window and (where known) pricing are populated automatically for the
default model.

#### `groq` — Groq

- Env: `GROQ_API_KEY`. Base URL defaults to
  `https://api.groq.com/openai/v1`. Default model:
  `llama-3.3-70b-versatile`.
- Performance note: Groq's LPU hardware serves Llama 3.x and Mixtral at
  500-800 tokens/sec — an order of magnitude above typical GPU hosts.

```toml
[providers.groq]
api_key_env = "GROQ_API_KEY"
default_model = "llama-3.3-70b-versatile"
```

#### `xai` — xAI Grok

- Env: `XAI_API_KEY`. Base URL defaults to `https://api.x.ai/v1`.
  Default model: `grok-4`.

```toml
[providers.xai]
api_key_env = "XAI_API_KEY"
default_model = "grok-4"
```

#### `deepseek` — DeepSeek

- Env: `DEEPSEEK_API_KEY`. Base URL defaults to
  `https://api.deepseek.com/v1`. Default model: `deepseek-chat`.
- Cost note: DeepSeek's direct API serves DeepSeek-V3 and R1 at roughly
  $0.27/$1.10 per Mtok input/output — an order of magnitude cheaper than
  equivalent-quality models on the frontier vendors.

```toml
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"
default_model = "deepseek-chat"
```

### Single-vendor OpenAI-compatible (light preset)

These four providers are wired up but lack curated model rows in the
registry; context window and pricing fall back to the generic estimate
(see the accounting table below) until you override `default_model` with
a model that is in the registry. The model catalog for each lives at
<https://models.dev/>.

#### `mistral` — Mistral La Plateforme

```toml
[providers.mistral]
api_key_env = "MISTRAL_API_KEY"
default_model = "mistral-large-latest"
# base_url defaults to https://api.mistral.ai/v1
```

#### `together` — Together AI

- Hosts the Llama family plus DeepSeek, Qwen, and partner checkpoints.

```toml
[providers.together]
api_key_env = "TOGETHER_API_KEY"
default_model = "meta-llama/Llama-3.3-70B-Instruct-Turbo"
# base_url defaults to https://api.together.xyz/v1
```

#### `fireworks` — Fireworks AI

- Hosts Llama, Mixtral, and DeepSeek with function-calling fine-tunes.

```toml
[providers.fireworks]
api_key_env = "FIREWORKS_API_KEY"
default_model = "accounts/fireworks/models/llama-v3p3-70b-instruct"
# base_url defaults to https://api.fireworks.ai/inference/v1
```

#### `cerebras` — Cerebras

- Performance note: Cerebras serves Llama 3.1 70B at roughly 1800
  tokens/sec — currently the fastest hosted open-weight inference.

```toml
[providers.cerebras]
api_key_env = "CEREBRAS_API_KEY"
default_model = "llama-3.3-70b"
# base_url defaults to https://api.cerebras.ai/v1
```

### Custom OpenAI-compatible endpoint

#### `openai_compatible` — Custom

Any service that speaks `POST /chat/completions` with a Bearer token
works through this preset. Common targets:

- Microsoft Foundry (Azure AI Studio) serverless deployment — `base_url`
  is your Foundry endpoint, e.g.
  `https://your-deployment.eastus2.models.ai.azure.com/v1`.
- Cloudflare Workers AI — `base_url` contains your account id.
- LM Studio, vLLM, llama.cpp server — point `base_url` at the local
  endpoint (LM Studio defaults to `http://127.0.0.1:1234/v1`, vLLM to
  `http://localhost:8000/v1`, llama.cpp to `http://localhost:8080`).
- Self-hosted LiteLLM proxy — `base_url` is your deployment.
- Cohere — partial compatibility; tool calling may differ.

```toml
[providers.openai_compatible]
api_key_env = "CUSTOM_API_KEY"
base_url = "https://api.cloudflare.com/client/v4/accounts/ACCOUNT_ID/ai/v1"
default_model = "@cf/meta/llama-3.3-70b-instruct-fp8-fast"
```

## Startup detection

On first interactive startup, when no provider/model choice has been
saved, Squeezy detects available API-key environment variables and
local Ollama availability. Aggregators (OpenRouter / Vercel / PortKey)
are listed first because one key gives access to many models. The
picker also detects `GROQ_API_KEY`, `XAI_API_KEY`, `DEEPSEEK_API_KEY`,
`MISTRAL_API_KEY`, `TOGETHER_API_KEY`, `FIREWORKS_API_KEY`,
`CEREBRAS_API_KEY`, `AI_GATEWAY_API_KEY`, `PORTKEY_API_KEY`, plus
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, and the Azure
variants. It asks for provider/token, model, and supported model
options (e.g. OpenAI reasoning effort), then saves only the environment
variable name and the selected defaults to `~/.squeezy/settings.toml`.
Secret token values are never written. Use `--no-default` to run the
selector again.

For OpenAI, Anthropic, Google, Azure OpenAI, and every OpenAI-compatible
preset, the environment variable named by `api_key_env` always wins.
When the environment variable is absent, Squeezy asks Keychain for the
configured service. On non-macOS hosts this fallback is not available.

## CLI

```sh
cargo run -p squeezy -- --list-providers
cargo run -p squeezy -- --list-models
cargo run -p squeezy -- --provider openrouter --model anthropic/claude-opus-4-7 --prompt "hello"
cargo run -p squeezy -- --provider groq --model llama-3.1-8b-instant --prompt "hello"
cargo run -p squeezy -- --provider ollama --model qwen3 --prompt "hello"
```

Existing env overrides remain supported: `SQUEEZY_PROVIDER`, `SQUEEZY_MODEL`,
`SQUEEZY_PROFILE`, the provider-specific base URL variables
(`OPENROUTER_BASE_URL`, `VERCEL_BASE_URL`, `PORTKEY_BASE_URL`, `GROQ_BASE_URL`,
`XAI_BASE_URL`, `DEEPSEEK_BASE_URL`, `MISTRAL_BASE_URL`, `TOGETHER_BASE_URL`,
`FIREWORKS_BASE_URL`, `CEREBRAS_BASE_URL`), and the provider API-key-env
variables.

## Built-in model accounting metadata

Squeezy keeps seed metadata for default models so local accounting
surfaces can estimate assembled-request size without starting a model
turn:

| Provider | Default model | Context window | Max output |
| --- | --- | ---: | ---: |
| `openai` | `gpt-5.5` | 400,000 | 128,000 |
| `azure_openai` | `gpt-5.5` | 400,000 | 128,000 |
| `anthropic` | `claude-opus-4-7` | 200,000 | 64,000 |
| `bedrock` | `anthropic.claude-haiku-4-5-20251001-v1:0` | 200,000 | 64,000 |
| `google` | `gemini-2.5-pro` | 1,048,576 | 65,536 |
| `ollama` | `qwen3-coder` | Runtime | Runtime |
| `openrouter` | `anthropic/claude-opus-4-7` | 200,000 | 64,000 |
| `vercel` | `anthropic/claude-opus-4-7` | 200,000 | 64,000 |
| `groq` | `llama-3.3-70b-versatile` | 131,072 | 32,768 |
| `xai` | `grok-4` | 256,000 | 32,768 |
| `deepseek` | `deepseek-chat` | 131,072 | 8,192 |

Light-preset providers (`portkey`, `mistral`, `together`, `fireworks`,
`cerebras`) and the `openai_compatible` custom preset fall back to a
generic 272K context / 64K max-output estimate until you set
`default_model` to a model that exists in the curated registry.

Ollama limits are local model metadata. Squeezy tries `/api/show` and
uses `model_info.*.context_window` or `num_ctx` when available;
otherwise the context window remains unknown. Custom model ids are also
treated as unknown until added to the registry or reported by the local
provider.

Pricing values are seed metadata for routing and telemetry, not billing
authority. Aggregator entries leave pricing `null` because effective
price depends on the route and the aggregator's markup; the token meter
still reports usage. Refresh pricing from provider pages when changing
defaults. Context usage values are local estimates. They are meant to
explain Squeezy's assembled request and are not provider billing
counters.

## Provider Status

- `openai`: OpenAI Responses streaming, function tools, cached-token usage, response state.
- `anthropic`: Anthropic Messages streaming, function tools, cache read/write usage.
- `google`: Gemini `streamGenerateContent` SSE streaming, function declarations, function calls, usage metadata.
- `azure_openai`: Azure OpenAI Responses-compatible streaming with `api-key` auth and `api-version`.
- `ollama`: Local `/api/chat` NDJSON streaming with function tool schemas and zero-dollar pricing.
- `bedrock`: AWS SDK Bedrock Runtime `ConverseStream` transport, AWS default credential chain, region/base-url configuration, text streaming, tool use/tool results, and usage metadata.
- `openrouter` / `vercel` / `portkey` / `groq` / `xai` / `deepseek` / `mistral` / `together` / `fireworks` / `cerebras` / `openai_compatible`: OpenAI-compatible `POST /chat/completions` streaming with Bearer auth, function-tool schemas, and `usage` extraction. OpenRouter ships default `HTTP-Referer` / `X-Title` headers for traffic attribution.

## Live integration tests

Each hosted provider has an opt-in "costly" integration test under
`crates/squeezy-llm/tests/*_costly.rs`. They are gated by the `costly-tests`
Cargo feature and `SQUEEZY_RUN_COSTLY_TESTS=1`, plus the relevant provider
credential. CI does not run them. Local invocation:

```sh
SQUEEZY_RUN_COSTLY_TESTS=1 \
  OPENROUTER_API_KEY=sk-... \
  cargo test -p squeezy-llm --features costly-tests \
  --test openrouter_costly -- --include-ignored
```

The same shape works for `vercel_costly`, `portkey_costly`, `groq_costly`,
`xai_costly`, `deepseek_costly`, `google_costly`, `azure_openai_costly`,
`bedrock_costly`, plus the existing `openai_costly` and `anthropic_costly`.

A free Ollama smoke test lives at `crates/squeezy-llm/tests/ollama_smoke.rs`.
It silently skips when no daemon is reachable; set `SQUEEZY_OLLAMA_SMOKE=1`
to fail loudly on a host that's expected to have Ollama running.
