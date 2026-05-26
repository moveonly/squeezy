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

## Fastest start: pick an aggregator

If you don't already have a vendor account, **OpenRouter** is the shortest
path: one credit account routes to every frontier model under a single key.
Vercel AI Gateway and PortKey work the same way and are listed below.

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

## Settings File

```toml
[model]
provider = "openrouter"
profile = "balanced"
model = ""

# ── Aggregators (one credit, many models) ──────────────────────────────────

[providers.openrouter]
api_key_env = "OPENROUTER_API_KEY"
base_url = "https://openrouter.ai/api/v1"
default_model = "anthropic/claude-opus-4-7"

[providers.openrouter.headers]
# Optional — OpenRouter uses these for traffic attribution. Squeezy ships
# defaults that point at the Squeezy GitHub; override if you'd rather show
# your own deployment.
"HTTP-Referer" = "https://github.com/esqueezy/squeezy"
"X-Title" = "Squeezy"

[providers.vercel]
api_key_env = "AI_GATEWAY_API_KEY"
base_url = "https://ai-gateway.vercel.sh/v1"
default_model = "anthropic/claude-opus-4-7"

[providers.portkey]
api_key_env = "PORTKEY_API_KEY"
base_url = "https://api.portkey.ai/v1"
# api_key = "..."   # or set via `squeezy auth set portkey`

# PortKey is a gateway, not a model host. There are four ways to tell it
# which upstream to call. Pick one — squeezy passes the model id and any
# extra headers through verbatim.
#
# 1. Integration-prefixed model id (most common for accounts with
#    "Integrations" attached in the PortKey workspace):
#       model = "@openrouter/qwen/qwen3.6-35b-a3b"
#       model = "@open-ai/gpt-4o-mini"
#    Hit GET https://api.portkey.ai/v1/models with your key to list
#    every model id available to your account.
#
# 2. Virtual Key — created in PortKey, bundles the upstream's API key:
#       model = "claude-opus-4-7"
#       [providers.portkey.headers]
#       "x-portkey-virtual-key" = "vk-xxxxxxxxxxxx"
#
# 3. Config — created in PortKey, defines model aliases + routing:
#       model = "sonnet-latest"           # alias defined in the Config
#       [providers.portkey.headers]
#       "x-portkey-config" = "your-config-id"
#    (If the Config is already attached to your User key in PortKey,
#    you don't need the header — just the model alias.)
#
# 4. Direct provider header (rare; for a generic PortKey key when the
#    upstream credentials are configured PortKey-side under a default
#    provider):
#       [providers.portkey.headers]
#       "x-portkey-provider" = "anthropic"

# ── First-party vendor APIs (single vendor) ───────────────────────────────

[providers.openai]
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
default_model = "gpt-5.5"

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://api.anthropic.com/v1"
default_model = "claude-opus-4-7"

[providers.google]
api_key_env = "GEMINI_API_KEY"
base_url = "https://generativelanguage.googleapis.com/v1beta"
default_model = "gemini-2.5-pro"

# ── Cloud-platform hosts ──────────────────────────────────────────────────
# Models hosted on a cloud platform's infrastructure behind that platform's
# IAM rather than a vendor API key.
#
#   * Amazon Bedrock — AWS multi-vendor catalog. Uses the AWS credential chain.
#   * Azure OpenAI Service — OpenAI-only slice of Azure's model catalog.
#     Use the `openai_compatible` preset further down with the Foundry
#     serverless endpoint for Azure AI Foundry's broader catalog.
#   * Google Vertex AI — Gemini and partner models via OAuth2 access tokens.

[providers.bedrock]
region = "us-east-1"
default_model = "anthropic.claude-haiku-4-5-20251001-v1:0"

[providers.azure_openai]
api_key_env = "AZURE_OPENAI_API_KEY"
base_url = "https://RESOURCE.openai.azure.com/openai/v1"
api_version = "v1"
default_model = "gpt-5.5"

[providers.vertex]
# Either provide a short-lived OAuth2 access token directly, or point at a
# service-account JSON path; Squeezy refreshes the token transparently in the
# latter case.
api_key_env = "VERTEX_ACCESS_TOKEN"
# service_account_json = "/path/to/key.json"
vertex_project = "my-gcp-project"
vertex_location = "us-central1"
default_model = "google/gemini-2.5-pro"
# base_url is templated from project + location to
# https://{location}-aiplatform.googleapis.com/v1/projects/{project}/locations/{location}/endpoints/openapi

# ── Local runtime ─────────────────────────────────────────────────────────

[providers.ollama]
base_url = "http://localhost:11434/api"
default_model = "qwen3-coder"

# ── Single-vendor OpenAI-compatible (full preset) ─────────────────────────

[providers.groq]
api_key_env = "GROQ_API_KEY"
default_model = "llama-3.3-70b-versatile"
# base_url defaults to https://api.groq.com/openai/v1

[providers.xai]
api_key_env = "XAI_API_KEY"
default_model = "grok-4"
# base_url defaults to https://api.x.ai/v1

[providers.deepseek]
api_key_env = "DEEPSEEK_API_KEY"
default_model = "deepseek-chat"
# base_url defaults to https://api.deepseek.com/v1

# ── Single-vendor OpenAI-compatible (light preset) ────────────────────────
# No curated models in the registry; cost/limit estimates fall back to
# generic OpenAI-compatible defaults. Override `default_model` per-section.

[providers.mistral]
api_key_env = "MISTRAL_API_KEY"
default_model = "mistral-large-latest"
# base_url defaults to https://api.mistral.ai/v1

[providers.together]
api_key_env = "TOGETHER_API_KEY"
default_model = "meta-llama/Llama-3.3-70B-Instruct-Turbo"
# base_url defaults to https://api.together.xyz/v1

[providers.fireworks]
api_key_env = "FIREWORKS_API_KEY"
default_model = "accounts/fireworks/models/llama-v3p3-70b-instruct"
# base_url defaults to https://api.fireworks.ai/inference/v1

[providers.cerebras]
api_key_env = "CEREBRAS_API_KEY"
default_model = "llama-3.3-70b"
# base_url defaults to https://api.cerebras.ai/v1

# ── Custom OpenAI-compatible endpoint ─────────────────────────────────────
# Any service that speaks `POST /chat/completions` with a Bearer token
# works through the `openai_compatible` preset. Examples:
#
#   * Microsoft Foundry (Azure AI Studio) serverless deployment — base_url is
#     your Foundry endpoint, e.g.
#     https://your-deployment.eastus2.models.ai.azure.com/v1
#   * Cloudflare Workers AI — base_url contains your account id.
#   * Self-hosted LiteLLM proxy — base_url is your deployment.
#   * Cohere — partial compatibility; tool calling may differ.

[providers.openai_compatible]
api_key_env = "CUSTOM_API_KEY"
base_url = "https://api.cloudflare.com/client/v4/accounts/ACCOUNT_ID/ai/v1"
default_model = "@cf/meta/llama-3.3-70b-instruct-fp8-fast"
```

`model = ""` means Squeezy uses the selected provider default. `profile` is
recorded and exposed to telemetry/model selection surfaces; current accepted
values are `cheap`, `balanced`, and `strong`.

On first interactive startup, when no provider/model choice has been saved,
Squeezy detects available API-key environment variables and local Ollama
availability. Aggregators (OpenRouter / Vercel / PortKey) are listed first
because one key gives access to many models. The picker also detects
`GROQ_API_KEY`, `XAI_API_KEY`, `DEEPSEEK_API_KEY`, `MISTRAL_API_KEY`,
`TOGETHER_API_KEY`, `FIREWORKS_API_KEY`, `CEREBRAS_API_KEY`,
`AI_GATEWAY_API_KEY`, `PORTKEY_API_KEY`, plus the existing
`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, and Azure variants.
It asks for provider/token, model, and supported model options (e.g. OpenAI
reasoning effort), then saves only the environment variable name and the
selected defaults to `~/.squeezy/settings.toml`. Secret token values are never
written. Use `--no-default` to run the selector again.

For OpenAI, Anthropic, Google, Azure OpenAI, and every OpenAI-compatible
Password entry. The environment variable named by `api_key_env` always wins.
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

## Built-In Model Accounting Metadata

Squeezy keeps seed metadata for default models so local accounting surfaces can
estimate assembled-request size without starting a model turn:

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
`cerebras`) and the `openai_compatible` custom preset fall back to a generic
272K context / 64K max-output estimate until you set `default_model` to a
model that exists in the curated registry.

Ollama limits are local model metadata. Squeezy tries `/api/show` and uses
`model_info.*.context_window` or `num_ctx` when available; otherwise the
context window remains unknown. Custom model ids are also treated as unknown
until added to the registry or reported by the local provider.

## Provider Status

- `openai`: OpenAI Responses streaming, function tools, cached-token usage, response state.
- `anthropic`: Anthropic Messages streaming, function tools, cache read/write usage.
- `google`: Gemini `streamGenerateContent` SSE streaming, function declarations, function calls, usage metadata.
- `azure_openai`: Azure OpenAI Responses-compatible streaming with `api-key` auth and `api-version`.
- `ollama`: Local `/api/chat` NDJSON streaming with function tool schemas and zero-dollar pricing.
- `bedrock`: AWS SDK Bedrock Runtime `ConverseStream` transport, AWS default credential chain, region/base-url configuration, text streaming, tool use/tool results, and usage metadata.
- `openrouter` / `vercel` / `portkey` / `groq` / `xai` / `deepseek` / `mistral` / `together` / `fireworks` / `cerebras` / `openai_compatible`: OpenAI-compatible `POST /chat/completions` streaming with Bearer auth, function-tool schemas, and `usage` extraction. OpenRouter ships default `HTTP-Referer` / `X-Title` headers for traffic attribution.

Pricing values are seed metadata for routing and telemetry, not billing
authority. Aggregator entries leave pricing `null` because effective price
depends on the route and the aggregator's markup; the token meter still
reports usage. Refresh pricing from provider pages when changing defaults.
Context usage values are local estimates. They are meant to explain Squeezy's
assembled request and are not provider billing counters.

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
