# Provider And Model Configuration

The full configuration model, source precedence, templates, and inspection
commands are documented in [`CONFIGURATION.md`](CONFIGURATION.md). This page
focuses on provider-specific fields.

Squeezy resolves provider settings from the same chain as the rest of the
configuration system. From highest to lowest precedence:

1. CLI flags
2. Environment variables
3. Per-repo user settings at `~/.squeezy/projects/<repo-id>/settings.toml`
4. Project `squeezy.toml` (nearest ancestor)
5. User `~/.squeezy/settings.toml`
6. Built-in defaults

See [`CONFIGURATION.md`](CONFIGURATION.md) for the merging rules and the
`config inspect` / `doctor` source reporting. The default user settings
path can be overridden with `SQUEEZY_SETTINGS_PATH`.

## Choose your provider

Squeezy ships 27 provider ids:
`openai`, `openai_codex`, `github_copilot`, `anthropic`, `google`,
`azure_openai`, `bedrock`, `ollama`, `openrouter`, `vercel`, `portkey`, `groq`,
`xai`, `deepseek`, `vertex`, `mistral`, `together`, `fireworks`, `cerebras`,
`deepinfra`, `baseten`, `lmstudio`, `vllm`, `llamacpp`,
`cloudflare_workers_ai`, `cloudflare_ai_gateway`, and `openai_compatible`.
Use the decision tree below; per-provider sections follow.

- **Don't have any vendor account?** Start with **OpenRouter**. One credit
  account routes to every frontier model under a single key.
- **Want frontier-quality on a single vendor bill?** Use the first-party
  preset: `openai`, `anthropic`, or `google`.
- **Want subscription-backed auth?** Use `openai_codex` for OpenAI Codex OAuth
  or `github_copilot` for GitHub Copilot OAuth.
- **Already on a cloud platform?** Use the platform-IAM preset:
  `bedrock` (AWS), `azure_openai` (Azure), or `vertex` (GCP).
- **Want maximum tokens-per-second on open-weight models?** Try `groq` or
  `cerebras` (Llama 3.x at 500-1800 tok/s).
- **Want the cheapest open-weight access?** Try `deepseek` (its own
  DeepSeek-V3 / R1) or `together` / `fireworks` / `deepinfra` (Llama,
  Qwen, Mixtral).
- **Running models locally?** Use `ollama`, `lmstudio`, `vllm`, or `llamacpp`.
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
- Default model: `anthropic/claude-opus-4.7`.
- Same OpenAI-compatible wire as OpenRouter; pricing and per-model
  capability come from Vercel's gateway catalog.

```toml
[providers.vercel]
api_key_env = "AI_GATEWAY_API_KEY"
default_model = "anthropic/claude-opus-4.7"
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

#### `openai_codex` — OpenAI Codex OAuth

- Auth: `squeezy auth openai-codex login`. Uses the OpenAI Responses wire with
  tokens stored in Squeezy's local auth file rather than an API key env var.
- Model metadata is curated under the OpenAI provider family; override the model
  the same way you would for `openai`.

```toml
[model]
provider = "openai_codex"
```

#### `github_copilot` — GitHub Copilot OAuth

- Auth: `squeezy auth github-copilot login`. Base URL is token/domain derived.
- Useful when your organization grants Copilot model access through GitHub
  rather than direct vendor API keys.

```toml
[model]
provider = "github_copilot"
```

#### `anthropic` — Anthropic

- Env: `ANTHROPIC_API_KEY`. Base URL: `https://api.anthropic.com/v1`.
- Default model: `claude-sonnet-4-6`. Uses the Anthropic Messages wire
  (streaming, function tools, cache read/write usage).

```toml
[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
default_model = "claude-sonnet-4-6"
```

#### `google` — Google Gemini

- Env: provider config defaults to `SQUEEZY_GOOGLE_KEY`; Squeezy also accepts
  `GOOGLE_API_KEY`, and first-start detection recognizes `GEMINI_API_KEY`.
  Base URL: `https://generativelanguage.googleapis.com/v1beta`.
- Default model: `gemini-2.5-pro`. Uses Gemini `streamGenerateContent`
  SSE streaming with function declarations, function calls, and usage
  metadata.

```toml
[providers.google]
api_key_env = "SQUEEZY_GOOGLE_KEY"
default_model = "gemini-2.5-pro"
```

### Cloud-platform hosts

Models hosted on a cloud platform's infrastructure behind that
platform's IAM rather than a vendor API key.

#### `bedrock` — Amazon Bedrock

- AWS multi-vendor catalog. Uses the AWS default credential chain (no
  `api_key_env`). Default model:
  `anthropic.claude-sonnet-4-6`. Transport is the AWS SDK
  Bedrock Runtime `ConverseStream`.

```toml
[providers.bedrock]
region = "us-east-1"
default_model = "anthropic.claude-sonnet-4-6"
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
  `vertex_project` + `vertex_location`. Default model:
  `google/gemini-3.1-pro-preview`.

```toml
[providers.vertex]
api_key_env = "VERTEX_ACCESS_TOKEN"
# service_account_json = "/path/to/key.json"
vertex_project = "my-gcp-project"
vertex_location = "us-central1"
default_model = "google/gemini-3.1-pro-preview"
```

### Local and self-hosted runtimes

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

#### `lmstudio` — LM Studio

- Base URL defaults to LM Studio's OpenAI-compatible local server.
- No API key is required unless your local server requires one.

```toml
[providers.lmstudio]
base_url = "http://127.0.0.1:1234/v1"
default_model = "local-model"
```

#### `vllm` — vLLM

```toml
[providers.vllm]
base_url = "http://localhost:8000/v1"
default_model = "served-model"
```

#### `llamacpp` — llama.cpp server

```toml
[providers.llamacpp]
base_url = "http://localhost:8080/v1"
default_model = "local-model"
```

### Single-vendor OpenAI-compatible (full preset)

These providers have curated model rows in the registry — context
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
  Default model: `grok-4.3`.

```toml
[providers.xai]
api_key_env = "XAI_API_KEY"
default_model = "grok-4.3"
```

#### `deepseek` — DeepSeek

- Env: `DEEPSEEK_API_KEY`. Base URL defaults to
  `https://api.deepseek.com/v1`. Default model: `deepseek-v4-flash`.
- Cost note: DeepSeek's direct API serves DeepSeek-V3 and R1 at roughly
  $0.27/$1.10 per Mtok input/output — an order of magnitude cheaper than
  equivalent-quality models on the frontier vendors.

```toml
[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"
default_model = "deepseek-v4-flash"
```

The `vertex` preset above is also a full-tier OpenAI-compatible provider for
registry/accounting purposes.

### Single-vendor OpenAI-compatible (light preset)

These providers are wired up but lack full-tier curated-test coverage in the
registry; context window and pricing fall back to the generic estimate
(see the accounting table below) until you override `default_model` with
a model that is in the registry. The model catalog for each lives at
<https://models.dev/>.

#### `mistral` — Mistral La Plateforme

```toml
[providers.mistral]
api_key_env = "MISTRAL_API_KEY"
default_model = "mistral-large-2512"
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
default_model = "accounts/fireworks/models/llama-v4-scout-instruct"
# base_url defaults to https://api.fireworks.ai/inference/v1
```

#### `cerebras` — Cerebras

- Performance note: Cerebras serves Llama 3.1 70B at roughly 1800
  tokens/sec — currently the fastest hosted open-weight inference.

```toml
[providers.cerebras]
api_key_env = "CEREBRAS_API_KEY"
default_model = "gpt-oss-120b"
# base_url defaults to https://api.cerebras.ai/v1
```

#### `deepinfra` — DeepInfra

- Env: `DEEPINFRA_API_KEY`. OpenAI-compatible hosted open-weight models.

```toml
[providers.deepinfra]
api_key_env = "DEEPINFRA_API_KEY"
default_model = "meta-llama/Llama-4-Scout-17B-128E-Instruct"
```

#### `baseten` — Baseten

- Env: `BASETEN_API_KEY`. Baseten deployment URLs include a deployment id; set
  `deployment_id` in `[providers.baseten]` or `BASETEN_DEPLOYMENT_ID`.

```toml
[providers.baseten]
api_key_env = "BASETEN_API_KEY"
deployment_id = "deployment-id"
default_model = "moonshotai/kimi-k2.6-instruct"
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

#### `cloudflare_workers_ai` — Cloudflare Workers AI

- Env: `CLOUDFLARE_API_TOKEN` (or the provider's configured `api_key_env`).
- Requires `cloudflare_account_id`; the base URL is derived from that account.

```toml
[providers.cloudflare_workers_ai]
api_key_env = "CLOUDFLARE_API_TOKEN"
cloudflare_account_id = "ACCOUNT_ID"
default_model = "@cf/meta/llama-3.3-70b-instruct-fp8-fast"
```

#### `cloudflare_ai_gateway` — Cloudflare AI Gateway

- Env: `CLOUDFLARE_API_TOKEN`. Requires `cloudflare_account_id`; `cloudflare_gateway_id`
  defaults to `default` when omitted.

```toml
[providers.cloudflare_ai_gateway]
api_key_env = "CLOUDFLARE_API_TOKEN"
cloudflare_account_id = "ACCOUNT_ID"
cloudflare_gateway_id = "default"
default_model = "@cf/meta/llama-3.3-70b-instruct-fp8-fast"
```

## Startup detection

On first interactive startup, when no provider/model choice has been
saved, Squeezy offers first-party providers, Azure/Bedrock/Vertex platform
hosts, non-custom OpenAI-compatible presets, local Ollama, and OAuth-backed
providers whose local auth state is present. Hosted choices can use cached
live-discovered model catalogs from `squeezy refresh-models`; otherwise the
picker falls back to built-in defaults. It asks for provider, model, and
supported model options, then saves only environment variable names and selected
defaults to `~/.squeezy/settings.toml`. Secret token values are never written.
Use `--no-default` to run the selector again.

API key resolution is shared by hosted providers: inline `api_key`, local
credentials file (`~/.squeezy/credentials.json` or `SQUEEZY_CREDENTIALS_FILE`),
the configured `api_key_env`, provider fallback env vars such as
`GOOGLE_API_KEY`/`CLOUDFLARE_API_TOKEN`, then `SQUEEZY_CREDENTIALS_JSON`.
Use `squeezy auth set/list/status/remove` for file-backed API keys and
`squeezy auth openai-codex login`, `squeezy auth github-copilot login`, or
`squeezy auth anthropic login` for OAuth-backed providers.

## CLI

```sh
squeezy providers list
squeezy providers list --configured
squeezy providers info openrouter
squeezy refresh-models
squeezy --provider openrouter --model anthropic/claude-opus-4-7 --prompt "hello"
squeezy --provider groq --model llama-3.1-8b-instant --prompt "hello"
squeezy --provider ollama --model qwen3 --prompt "hello"
```

Existing env overrides remain supported: `SQUEEZY_PROVIDER`, `SQUEEZY_MODEL`,
`SQUEEZY_PROFILE`, the provider-specific base URL variables
(`OPENROUTER_BASE_URL`, `VERCEL_BASE_URL`, `PORTKEY_BASE_URL`, `GROQ_BASE_URL`,
`XAI_BASE_URL`, `DEEPSEEK_BASE_URL`, `MISTRAL_BASE_URL`, `TOGETHER_BASE_URL`,
`FIREWORKS_BASE_URL`, `CEREBRAS_BASE_URL`, `DEEPINFRA_BASE_URL`,
`BASETEN_BASE_URL`, `LMSTUDIO_BASE_URL`, `VLLM_BASE_URL`,
`LLAMACPP_BASE_URL`), provider-specific Cloudflare ids, and the provider
API-key-env variables.

## Built-in model accounting metadata

Squeezy keeps seed metadata for default models so local accounting
surfaces can estimate assembled-request size without starting a model
turn:

| Provider | Default model | Context window | Max output |
| --- | --- | ---: | ---: |
| `openai` | `gpt-5.5` | 400,000 | 128,000 |
| `azure_openai` | `gpt-5.5` | 400,000 | 128,000 |
| `anthropic` | `claude-sonnet-4-6` | 200,000 | 64,000 |
| `bedrock` | `anthropic.claude-sonnet-4-6` | 200,000 | 64,000 |
| `google` | `gemini-2.5-pro` | 1,048,576 | 65,536 |
| `ollama` | `qwen3-coder` | Runtime | Runtime |
| `openrouter` | `anthropic/claude-opus-4-7` | 200,000 | 64,000 |
| `vercel` | `anthropic/claude-opus-4.7` | 200,000 | 64,000 |
| `groq` | `llama-3.3-70b-versatile` | 131,072 | 32,768 |
| `xai` | `grok-4.3` | 256,000 | 32,768 |
| `deepseek` | `deepseek-v4-flash` | 131,072 | 8,192 |
| `vertex` | `google/gemini-3.1-pro-preview` | 1,048,576 | 65,536 |

Light-preset providers (`portkey`, `mistral`, `together`, `fireworks`,
`cerebras`, `deepinfra`, `baseten`, `lmstudio`, `vllm`, `llamacpp`,
`cloudflare_workers_ai`, `cloudflare_ai_gateway`) and the `openai_compatible`
custom preset fall back to a generic 272K context / 64K max-output estimate
until you set `default_model` to a model that exists in the curated registry.
`openai_codex` and `github_copilot` use OAuth-backed provider metadata from
their token flows.

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
- `openai_codex`: OpenAI Responses streaming with local OAuth token refresh.
- `github_copilot`: GitHub Copilot OAuth-backed model access.
- `openrouter` / `vercel` / `portkey` / `groq` / `xai` / `deepseek` / `mistral` / `together` / `fireworks` / `cerebras` / `deepinfra` / `baseten` / `lmstudio` / `vllm` / `llamacpp` / `cloudflare_workers_ai` / `cloudflare_ai_gateway` / `openai_compatible`: OpenAI-compatible `POST /chat/completions` streaming with Bearer auth, function-tool schemas, and `usage` extraction. OpenRouter ships default `HTTP-Referer` / `X-Title` headers for traffic attribution.

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
