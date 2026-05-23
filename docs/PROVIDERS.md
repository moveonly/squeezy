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
`config inspect` / `--health` source reporting. The default user settings
path can be overridden with `SQUEEZY_SETTINGS_PATH`.

## Settings File

```toml
[model]
provider = "openai"
profile = "balanced"
model = ""

[providers.openai]
api_key_env = "OPENAI_API_KEY"
base_url = "https://api.openai.com/v1"
default_model = "gpt-5-nano"

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://api.anthropic.com/v1"
default_model = "claude-3-5-haiku-20241022"

[providers.google]
api_key_env = "GEMINI_API_KEY"
base_url = "https://generativelanguage.googleapis.com/v1beta"
default_model = "gemini-2.5-flash-lite"

[providers.azure_openai]
api_key_env = "AZURE_OPENAI_API_KEY"
base_url = "https://RESOURCE.openai.azure.com/openai/v1"
api_version = "v1"
default_model = "gpt-5-nano"

[providers.bedrock]
region = "us-east-1"
default_model = "anthropic.claude-3-5-haiku-20241022-v1:0"

[providers.ollama]
base_url = "http://localhost:11434/api"
default_model = "qwen3"
```

`model = ""` means Squeezy uses the selected provider default. `profile` is recorded and exposed to telemetry/model selection surfaces; current accepted values are `cheap`, `balanced`, and `strong`.

## CLI

```sh
cargo run -p squeezy-cli -- --list-providers
cargo run -p squeezy-cli -- --list-models
cargo run -p squeezy-cli -- --provider ollama --model qwen3 --prompt "hello"
```

Existing env overrides remain supported: `SQUEEZY_PROVIDER`, `SQUEEZY_MODEL`, `SQUEEZY_PROFILE`, provider base URL variables, and provider API-key-env variables.

## Provider Status

- `openai`: OpenAI Responses streaming, function tools, cached-token usage, response state.
- `anthropic`: Anthropic Messages streaming, function tools, cache read/write usage.
- `google`: Gemini `streamGenerateContent` SSE streaming, function declarations, function calls, usage metadata.
- `azure_openai`: Azure OpenAI Responses-compatible streaming with `api-key` auth and `api-version`.
- `ollama`: Local `/api/chat` NDJSON streaming with function tool schemas and zero-dollar pricing.
- `bedrock`: Registered with model/capability/pricing metadata. The signed AWS ConverseStream transport is isolated behind the provider and currently returns a configuration error until a SigV4/event-stream implementation is enabled.

Pricing values are seed metadata for routing and telemetry, not billing authority. Refresh them from provider pricing pages when changing defaults.
