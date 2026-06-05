# Provider Support Research

This note is repo-local website research. It was written from current checkout
truth only: committed docs, `models.json`, CLI provider/auth code, core config,
LLM provider modules, OAuth modules, and tests. No network sources were used.
Do not treat this as a live provider catalog.

## Conservative Website Claims

- Squeezy is multi-provider, not OpenAI-only.
- The runtime can select native providers, subscription OAuth providers, and
  OpenAI-compatible endpoints.
- Provider routing stays within the configured provider. Cheap-turn routing
  uses the same provider's cheaper tier when Squeezy knows one; otherwise it
  falls back to the parent model.
- Cost accounting is best-effort and registry-backed. Dollar estimates require
  pricing metadata for the selected `(provider, model)` pair.
- Live provider tests are intentionally opt-in and must not be run from AI
  automation without explicit user approval and credentials.

Avoid stronger public claims such as "all providers have identical feature
support", "every provider has live test coverage", or "all listed providers have
priced models".

## Runtime-Selectable Providers

The active provider is resolved from CLI/provider env/config and defaults to
`openai` when unspecified. Core config currently recognizes:

| Provider | Runtime id / aliases | Auth route evident in repo | Notes |
| --- | --- | --- | --- |
| OpenAI | `openai` | Inline TOML API key, credential file, env (`SQUEEZY_OPENAI_KEY` / `OPENAI_API_KEY` fallback) | Default provider and default model family. Uses OpenAI Responses path. |
| Anthropic | `anthropic`, `claude` | Static API key route; `squeezy auth anthropic login` OAuth fallback when no static key resolves | OAuth module documents Claude Pro/Max token flow, and provider construction loads the OAuth source after static key resolution fails. |
| Google AI Studio | `google`, `gemini` | API key route (`SQUEEZY_GOOGLE_KEY` / `GOOGLE_API_KEY` fallback; costly test also accepts `GEMINI_API_KEY`) | Native Gemini provider. |
| Azure OpenAI | `azure`, `azure-openai`, `azure_openai` | API key route plus Entra bearer-token config surface | Per-deployment URL/model details are operator-specific. |
| AWS Bedrock | `bedrock`, `amazon-bedrock`, `amazon_bedrock` | AWS credential chain / region; no single inline API key | Doctor reports region and AWS credential-chain usage. |
| Ollama | `ollama`, `local` | No key by default | Local provider; optional reverse-proxy auth is not the default path. |
| OpenAI Codex / ChatGPT subscription | `openai-codex`, `openai_codex`, `chatgpt` | `squeezy auth openai-codex login/logout/status` | Uses a persisted OAuth token under `~/.squeezy/auth/openai-codex.json` and a ChatGPT backend URL. |
| GitHub Copilot | `github-copilot`, `github_copilot`, `copilot` | `squeezy auth github-copilot login/logout/status` | Device-code OAuth, token-derived request host, optional model policy enablement. |
| Faux | `faux`, `mock` | None | In-process scripted provider for tests/eval; do not market as a user provider. |

OpenAI-compatible presets are also runtime-selectable. Current preset ids:
`openrouter`, `vercel`, `portkey`, `groq`, `xai`, `deepseek`, `vertex`,
`mistral`, `together`, `fireworks`, `cerebras`, `deepinfra`, `baseten`,
`lmstudio`, `vllm`, `llamacpp`, `cloudflare_workers_ai`,
`cloudflare_ai_gateway`, and `openai_compatible`.

Local/self-hosted presets (`lmstudio`, `vllm`, `llamacpp`) default to local
OpenAI-compatible URLs and commonly run without auth. `openai_compatible` is
the custom escape hatch; the code warns because a project-local custom URL plus
a real API-key env var can exfiltrate credentials.

Vertex is a notable OpenAI-compatible preset: core can infer `use_oauth` from
`VERTEX_USE_OAUTH=1` or `GOOGLE_APPLICATION_CREDENTIALS` without
`VERTEX_ACCESS_TOKEN`, and the LLM provider builds a refreshable
`VertexOAuthSource` for that mode.

## Discovery and Model Metadata

There are three related surfaces:

- `crates/squeezy-core/src/lib.rs` decides what can be selected at runtime.
- `crates/squeezy-cli/src/providers.rs` powers `squeezy providers list/info`.
  It lists first-party base providers plus all OpenAI-compatible presets.
- `crates/squeezy-llm/src/models.json` is the curated model/pricing/capability
  registry. Unknown models fall back to generic capability/context estimates
  with no pricing.

Current `models.json` includes curated entries for these provider namespaces:
`openai`, `anthropic`, `google`, `azure_openai`, `bedrock`, `ollama`,
`openrouter`, `vercel`, `portkey`, `groq`, `xai`, `deepseek`, `vertex`,
`mistral`, `together`, `fireworks`, `cerebras`, `deepinfra`, `baseten`, and
`cloudflare_workers_ai`.

Not every runtime provider has curated model/pricing entries. In this checkout,
`openai_codex`, `github_copilot`, `lmstudio`, `vllm`, `llamacpp`,
`cloudflare_ai_gateway`, `openai_compatible`, and `faux` are selectable but do
not appear as priced provider namespaces in `models.json`.

Pricing is also incomplete for some curated aggregator entries. For example,
the registry has `openrouter`, `vercel`, and `portkey` models whose `pricing`
field is `null`, so dollar cost estimates can be unavailable even when token
usage is parsed.

## Authentication Surfaces

API-key resolution is layered:

1. Inline `[providers.<name>] api_key` from settings.
2. `~/.squeezy/credentials.json` fallback.
3. The configured env var.
4. Conventional fallback env var pairs such as `SQUEEZY_OPENAI_KEY` and
   `OPENAI_API_KEY`.
5. `SQUEEZY_CREDENTIALS_JSON`.

The CLI auth surface supports:

- `squeezy auth set/list/remove/status` for inline API-key management and
  resolution status.
- `squeezy auth anthropic login/logout/status` for Anthropic OAuth tokens.
- `squeezy auth openai-codex login/logout/status` for ChatGPT subscription
  OAuth.
- `squeezy auth github-copilot login/logout/status` for GitHub Copilot OAuth.

Caution: auth status and provider selection are not identical surfaces. Some
providers are env/config-driven rather than clearly covered by `auth set`, and
Bedrock/Ollama intentionally do not use a normal API-key path.

## Routing and Cost Behavior

Provider-aware cheap routing:

- `cheap_model_for` first honors `[providers.<name>].cheap_model`, then the
  legacy global small-fast model, then built-in per-provider cheap/judge tiers.
- Routing does not cross providers. If no cheap tier is known, Squeezy stays on
  the parent model.
- The reroute filter is per provider and skips already-cheap model families by
  name (`haiku`, `mini`, `nano`, `flash`, etc.).
- The LLM judge is intentionally short, uses no cache, has a 10 second timeout,
  and records its own provider cost when available.

Cost accounting:

- Provider stream parsers normalize usage into `CostSnapshot`.
- `estimate_cost` prices normalized token counts with `models.json`.
- If pricing is missing, dollar estimates are `None`; configured dollar caps
  can become unenforceable for that model and should be surfaced cautiously.
- Prompt-cache support is capability-gated by the registry. Anthropic-family
  routes can emit inline cache markers, OpenAI routes use `prompt_cache_key` /
  retention fields, and unsupported models receive no cache directive.

## Live Provider Tests

Do not run live provider tests by default. The costly test helper requires the
`costly-tests` Cargo feature and `SQUEEZY_RUN_COSTLY_TESTS=1`, and each provider
test also requires real provider credentials. Existing costly tests cover
OpenAI, Anthropic, Google, Azure OpenAI, Bedrock, OpenRouter, Vercel, PortKey,
Groq, xAI, DeepSeek, and Vertex. That is useful smoke coverage, not a guarantee
that every selectable provider has a current live test.

## Source References

- Runtime provider selection and defaults: `crates/squeezy-core/src/lib.rs:809`,
  `crates/squeezy-core/src/lib.rs:819`, `crates/squeezy-core/src/lib.rs:2521`,
  `crates/squeezy-core/src/lib.rs:2549`, `crates/squeezy-core/src/lib.rs:9932`.
- Provider discovery CLI: `crates/squeezy-cli/src/providers.rs:1`,
  `crates/squeezy-cli/src/providers.rs:188`, `crates/squeezy-cli/src/providers.rs:200`,
  `crates/squeezy-cli/src/providers.rs:237`.
- API-key auth CLI and resolution: `crates/squeezy-cli/src/auth.rs:21`,
  `crates/squeezy-cli/src/auth.rs:341`, `crates/squeezy-cli/src/auth.rs:724`,
  `crates/squeezy-llm/src/credentials.rs:36`.
- OAuth modules: `crates/squeezy-llm/src/oauth/anthropic.rs:1`,
  `crates/squeezy-llm/src/oauth/openai_codex.rs:1`,
  `crates/squeezy-llm/src/oauth/github_copilot.rs:1`.
- Provider construction: `crates/squeezy-llm/src/registry.rs:276`,
  `crates/squeezy-llm/src/registry.rs:424`, `crates/squeezy-llm/src/registry.rs:439`,
  `crates/squeezy-llm/src/anthropic.rs:127`, `crates/squeezy-llm/src/compatible.rs:100`.
- Model/pricing registry: `crates/squeezy-llm/src/models.json`,
  `crates/squeezy-llm/src/registry.rs:139`, `crates/squeezy-llm/src/registry.rs:347`,
  `crates/squeezy-llm/src/registry.rs:476`.
- Cheap routing: `crates/squeezy-core/src/lib.rs:56`,
  `crates/squeezy-core/src/lib.rs:100`, `crates/squeezy-core/src/lib.rs:119`,
  `crates/squeezy-agent/src/turn_router.rs:484`,
  `crates/squeezy-agent/src/turn_router.rs:583`,
  `crates/squeezy-agent/src/lib.rs:11015`.
- Prompt caching and token accounting docs/code:
  `docs/internal/cost-saving/01-provider-prompt-caching.md`,
  `docs/internal/cost-saving/10-token-accounting.md`,
  `crates/squeezy-llm/src/cache_policy.rs:1`.
- Live provider test gating: `crates/squeezy-llm/tests/common/mod.rs:1`,
  `crates/squeezy-llm/tests/openai_costly.rs:17`,
  `crates/squeezy-llm/tests/anthropic_costly.rs:17`,
  `crates/squeezy-llm/tests/google_costly.rs:15`.
