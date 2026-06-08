use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    env, fmt, fs,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod config_schema;
mod hardening;
pub mod settings_writer;
pub mod startup_trace;
pub use hardening::pre_main_hardening;

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-5.5";
/// ChatGPT Plus/Pro Codex backend. The protocol is OpenAI's Responses
/// API, but the requests go through the ChatGPT backend with the
/// subscription's account id stamped in `chatgpt-account-id`.
pub const DEFAULT_OPENAI_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const DEFAULT_OPENAI_CODEX_MODEL: &str = DEFAULT_OPENAI_MODEL;
/// Originator tag stamped on every Codex request so OpenAI can attribute
/// traffic to squeezy in their dashboards.
pub const DEFAULT_OPENAI_CODEX_ORIGINATOR: &str = "squeezy";
pub const DEFAULT_GITHUB_COPILOT_MODEL: &str = DEFAULT_OPENAI_MODEL;
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com/v1";
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
pub const DEFAULT_GOOGLE_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";
pub const DEFAULT_GOOGLE_MODEL: &str = "gemini-2.5-pro";
pub const DEFAULT_AZURE_OPENAI_BASE_URL: &str = "";
// C-13: the Azure provider targets the Responses endpoint
// (`/openai/v1/responses?api-version=…`), which Azure serves only under the
// `preview` api-version; `v1` returns a 4xx there. Default to `preview` so a
// bare `AZURE_OPENAI_*` config works out of the box. Operators can still pin a
// dated version (e.g. `2024-10-21`) via `providers.azure_openai.api_version`.
pub const DEFAULT_AZURE_OPENAI_API_VERSION: &str = "preview";
pub const DEFAULT_AZURE_OPENAI_MODEL: &str = DEFAULT_OPENAI_MODEL;
pub const DEFAULT_BEDROCK_REGION: &str = "us-east-1";
pub const DEFAULT_BEDROCK_MODEL: &str = "anthropic.claude-sonnet-4-6";
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434/api";
pub const DEFAULT_OLLAMA_MODEL: &str = "qwen3-coder";
/// Synthetic model id for the in-process faux provider. The faux
/// provider does not consult model registries during a request — this
/// is purely a label that flows through cost/logging surfaces.
pub const DEFAULT_FAUX_MODEL: &str = "faux-1";

// Small-fast-model defaults per provider. Used for low-stakes background calls
// (compaction summaries, classifier prompts, auto-approver) where flagship
// quality is wasted spend. Anthropic Haiku 4.5 is ~15x cheaper than Opus 4.7
// per input token; comparable shape on OpenAI Nano vs flagship.
pub const ANTHROPIC_SMALL_FAST_MODEL: &str = "claude-haiku-4-5-20251001";
pub const OPENAI_SMALL_FAST_MODEL: &str = "gpt-5.4-nano";
pub const GOOGLE_SMALL_FAST_MODEL: &str = "gemini-2.5-flash-lite";
pub const BEDROCK_SMALL_FAST_MODEL: &str = "anthropic.claude-haiku-4-5-20251001-v1:0";
pub const AZURE_OPENAI_SMALL_FAST_MODEL: &str = OPENAI_SMALL_FAST_MODEL;
pub const OPENROUTER_SMALL_FAST_MODEL: &str = "anthropic/claude-haiku-4-5";
pub const VERCEL_SMALL_FAST_MODEL: &str = "anthropic/claude-haiku-4.5";
pub const PORTKEY_SMALL_FAST_MODEL: &str = "anthropic/claude-haiku-4-5";
pub const VERTEX_SMALL_FAST_MODEL: &str = "google/gemini-2.5-flash";

/// Returns the built-in small-fast-model id for `provider`. `provider` is the
/// canonical short name (`provider_kind`/`LlmProvider::name`). Returns `None`
/// for providers that have no obvious cheaper tier (Ollama serves a single
/// local model; OpenAI-compatible light presets don't ship a curated cheap
/// model). Callers should treat `None` as "fall back to the parent model".
pub fn small_fast_model_for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "anthropic" => Some(ANTHROPIC_SMALL_FAST_MODEL),
        "openai" => Some(OPENAI_SMALL_FAST_MODEL),
        "google" => Some(GOOGLE_SMALL_FAST_MODEL),
        "azure_openai" => Some(AZURE_OPENAI_SMALL_FAST_MODEL),
        "bedrock" => Some(BEDROCK_SMALL_FAST_MODEL),
        "openrouter" => Some(OPENROUTER_SMALL_FAST_MODEL),
        "vercel" => Some(VERCEL_SMALL_FAST_MODEL),
        "portkey" => Some(PORTKEY_SMALL_FAST_MODEL),
        "vertex" => Some(VERTEX_SMALL_FAST_MODEL),
        _ => None,
    }
}

// Default turn-routing JUDGE model per provider — one notch ABOVE the cheap
// reroute tier. The judge classifies cheap-vs-parent, so a slightly stronger
// "mini" tier judges far more reliably than the cheapest "nano" tier while
// staying cheap; the reroute target stays the cheapest tier. Providers without
// a distinct mid tier (anthropic Haiku is already the small tier) reuse their
// small-fast model. Overridable per provider via `[providers.<p>].judge_model`.
pub const OPENAI_JUDGE_MODEL: &str = "gpt-5.4-mini";
pub const GOOGLE_JUDGE_MODEL: &str = "gemini-3.5-flash";
pub const AZURE_OPENAI_JUDGE_MODEL: &str = OPENAI_JUDGE_MODEL;

/// Returns the built-in default judge model id for `provider`, falling back to
/// the small-fast (reroute) tier when no distinct mid tier exists. `None` when
/// the provider has no cheaper tier at all (callers then judge on the parent).
pub fn judge_model_for_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some(OPENAI_JUDGE_MODEL),
        "google" => Some(GOOGLE_JUDGE_MODEL),
        "azure_openai" => Some(AZURE_OPENAI_JUDGE_MODEL),
        other => small_fast_model_for_provider(other),
    }
}

/// Built-in default reroute filter for `provider` — a single standard regex
/// matched against the parent model to decide whether an easy turn is worth
/// rerouting (the parent is rerouted when the regex matches). The defaults use a
/// negative lookahead to reroute every flagship while skipping the provider's
/// already-cheap tiers by NAME (not by exact model id), so they stay correct as
/// new models ship and scale to gateway/prefixed ids (e.g. bedrock
/// `anthropic.claude-haiku-…` still contains "haiku"). The leading `(?i)` is
/// case-insensitive (azure deployment names can be capitalised). Override per
/// provider via `[providers.<name>].expensive_models`, or globally via
/// `[routing].expensive_models`, with any regex.
pub fn default_reroute_filter(provider: &str) -> &'static str {
    match provider {
        // Bare tokens are safe inside a single namespace: no openai flagship
        // contains "mini"/"nano", no anthropic flagship "haiku", no google
        // flagship "flash".
        "openai" | "azure_openai" => "(?i)^(?!.*(nano|mini)).*",
        "anthropic" | "bedrock" => "(?i)^(?!.*haiku).*",
        "google" | "vertex" => "(?i)^(?!.*flash).*",
        // Gateways (openrouter/vercel/portkey/…) and unknown providers carry
        // prefixed, cross-vendor ids, so exclude every cheap-tier family. Anchor
        // each on a leading "-" so "-mini" doesn't also match "ge[mini]-2.5-pro".
        _ => "(?i)^(?!.*(-nano|-mini|-haiku|-flash|-lite)).*",
    }
}

/// Whether `parent` is eligible to be rerouted given `filter` — a single
/// standard regex (lookaround supported). The parent is eligible when the regex
/// matches; an empty filter reroutes any parent. An invalid regex falls back to
/// a case-insensitive substring test so a plain model name always works.
pub fn parent_is_reroute_eligible(parent: &str, filter: &str) -> bool {
    let filter = filter.trim();
    if filter.is_empty() {
        return true;
    }
    match fancy_regex::Regex::new(filter) {
        // `is_match` only errs on a backtrack-limit blowup, which short model
        // ids never hit; treat that as "not eligible" (stay on the parent).
        Ok(re) => re.is_match(parent).unwrap_or(false),
        Err(_) => parent.to_lowercase().contains(&filter.to_lowercase()),
    }
}

/// The reroute filter in effect for `provider`: per-provider override
/// (`[providers.<name>].expensive_models`, including an explicit empty string =
/// "reroute any") → non-empty global `[routing].expensive_models` → the
/// built-in per-provider default. Shared by the turn router and the config
/// screen so the displayed filter always matches what the router applies.
pub fn resolved_reroute_filter(config: &AppConfig, provider: &str) -> String {
    if let Some(filter) = config
        .providers
        .get(provider)
        .and_then(|p| p.expensive_models.clone())
    {
        return filter;
    }
    if !config.routing.expensive_models.is_empty() {
        return config.routing.expensive_models.clone();
    }
    default_reroute_filter(provider).to_string()
}

// Built-in turn-routing JUDGE prompts. All carry the same routing guidance
// (short, well-specified, mechanical → cheap; architectural / exploratory /
// debugging → parent) but differ in formatting cues per provider tier. Lives
// in core so the config screen can show "the prompt we're using" and the agent
// can dispatch it. A user `[providers.<p>].judge_prompt` overrides this.
pub const JUDGE_PROMPT_DEFAULT: &str = concat!(
    "You are a routing classifier deciding which LLM should handle a coding-agent turn. ",
    "The parent model is expensive but excellent at multi-step reasoning. The cheap model is fast and ",
    "inexpensive but weaker at ambiguous instructions and architectural judgement. ",
    "Reply with a SINGLE JSON object on one line, no markdown, no prose: ",
    "{\"route\":\"cheap\"|\"parent\",\"reason\":\"<short explanation>\"}.\n\n",
    "Choose 'cheap' when the request is well-specified, narrowly scoped, and mechanical — a single named ",
    "operation plus its targets (e.g. \"checkout branch X and run cargo test\", \"rename foo() to bar() in src/lib.rs\"). ",
    "Choose 'parent' when the request needs architectural reasoning, cross-file synthesis, exploratory ",
    "investigation, debugging, or judgement about trade-offs. When in doubt, choose 'parent'.",
);
pub const JUDGE_PROMPT_OPENAI: &str = concat!(
    "You are a routing classifier. Output ONLY a single JSON object on one line. ",
    "Do NOT include any prose, preamble, explanation, or trailing text. ",
    "Schema: {\"route\":\"cheap\"|\"parent\",\"reason\":\"<short explanation>\"}.\n\n",
    "The parent model is expensive but excellent at multi-step reasoning. The cheap model is fast but ",
    "weaker at ambiguous instructions and architectural judgement. ",
    "Choose 'cheap' when the request is well-specified, narrowly scoped, and mechanical — a single named ",
    "operation plus its targets (e.g. \"checkout branch X and run cargo test\", \"rename foo() to bar() in src/lib.rs\"). ",
    "Choose 'parent' when the request needs architectural reasoning, cross-file synthesis, exploratory ",
    "investigation, debugging, or judgement about trade-offs. When in doubt, choose 'parent'.",
);
pub const JUDGE_PROMPT_GOOGLE: &str = concat!(
    "You are a routing classifier. Reply with ONLY a single JSON object on one line — NO markdown fences, ",
    "NO code blocks, NO prose, NO commentary. ",
    "Schema: {\"route\":\"cheap\"|\"parent\",\"reason\":\"<short explanation>\"}.\n\n",
    "The parent model is expensive but excellent at multi-step reasoning. The cheap model is fast but ",
    "weaker at ambiguous instructions and architectural judgement. ",
    "Choose 'cheap' when the request is well-specified, narrowly scoped, and mechanical — a single named ",
    "operation plus its targets (e.g. \"checkout branch X and run cargo test\", \"rename foo() to bar() in src/lib.rs\"). ",
    "Choose 'parent' when the request needs architectural reasoning, cross-file synthesis, exploratory ",
    "investigation, debugging, or judgement about trade-offs. When in doubt, choose 'parent'.",
);

/// The built-in judge prompt for `provider`. A `[providers.<p>].judge_prompt`
/// override takes precedence (resolved at the use-site).
pub fn default_judge_prompt(provider: &str) -> &'static str {
    match provider {
        "openai" | "openai_codex" | "azure_openai" => JUDGE_PROMPT_OPENAI,
        "google" => JUDGE_PROMPT_GOOGLE,
        _ => JUDGE_PROMPT_DEFAULT,
    }
}

// OpenAI-compatible aggregators (full preset tier — curated models in models.json, dedicated costly test).
pub const DEFAULT_OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const DEFAULT_OPENROUTER_MODEL: &str = "anthropic/claude-sonnet-4.6";
pub const DEFAULT_VERCEL_AI_BASE_URL: &str = "https://ai-gateway.vercel.sh/v1";
pub const DEFAULT_VERCEL_AI_MODEL: &str = "anthropic/claude-sonnet-4.6";
pub const DEFAULT_PORTKEY_BASE_URL: &str = "https://api.portkey.ai/v1";
pub const DEFAULT_PORTKEY_MODEL: &str = "anthropic/claude-sonnet-4-6";
// OpenAI-compatible single-vendor (full preset tier).
pub const DEFAULT_GROQ_BASE_URL: &str = "https://api.groq.com/openai/v1";
pub const DEFAULT_GROQ_MODEL: &str = "llama-3.3-70b-versatile";
pub const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1";
pub const DEFAULT_XAI_MODEL: &str = "grok-4.3";
pub const DEFAULT_DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
pub const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-v4-flash";
// Google Cloud Vertex AI's OpenAI-compatible endpoint. The base URL is
// per-project + per-region, so users must set `vertex_project` and
// `vertex_location` (or override `base_url` directly).
pub const DEFAULT_VERTEX_LOCATION: &str = "us-central1";
pub const DEFAULT_VERTEX_MODEL: &str = "google/gemini-3.1-pro-preview";
// OpenAI-compatible single-vendor (light preset tier — no curated models, no dedicated costly test).
pub const DEFAULT_MISTRAL_BASE_URL: &str = "https://api.mistral.ai/v1";
pub const DEFAULT_MISTRAL_MODEL: &str = "mistral-large-2512";
pub const DEFAULT_TOGETHER_BASE_URL: &str = "https://api.together.xyz/v1";
pub const DEFAULT_TOGETHER_MODEL: &str = "meta-llama/Llama-3.3-70B-Instruct-Turbo";
pub const DEFAULT_FIREWORKS_BASE_URL: &str = "https://api.fireworks.ai/inference/v1";
pub const DEFAULT_FIREWORKS_MODEL: &str = "accounts/fireworks/models/llama-v4-scout-instruct";
pub const DEFAULT_CEREBRAS_BASE_URL: &str = "https://api.cerebras.ai/v1";
pub const DEFAULT_CEREBRAS_MODEL: &str = "gpt-oss-120b";
pub const DEFAULT_DEEPINFRA_BASE_URL: &str = "https://api.deepinfra.com/v1/openai";
pub const DEFAULT_DEEPINFRA_MODEL: &str = "meta-llama/Llama-4-Scout-17B-128E-Instruct";
pub const DEFAULT_BASETEN_BASE_URL: &str = "https://inference.baseten.co/v1";
pub const DEFAULT_BASETEN_MODEL: &str = "moonshotai/kimi-k2.6-instruct";
// OpenAI-compatible local self-hosted (light preset tier — loopback default,
// auth optional, model id depends on whatever the local server has loaded).
pub const DEFAULT_LMSTUDIO_BASE_URL: &str = "http://127.0.0.1:1234/v1";
pub const DEFAULT_VLLM_BASE_URL: &str = "http://127.0.0.1:8000/v1";
pub const DEFAULT_LLAMACPP_BASE_URL: &str = "http://127.0.0.1:8080/v1";
// Cloudflare Workers AI + AI Gateway. Both base URLs are per-account
// (and the Gateway preset additionally per-gateway), so the default
// templates carry `{account_id}` / `{gateway_id}` placeholders that get
// substituted out of the matching `OpenAiCompatibleConfig` fields before
// requests fire. The substitution lives in the LLM client
// (`squeezy-llm::compatible`) so any user override of `base_url` that
// keeps the placeholder syntax behaves the same as the bundled default.
pub const DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL: &str =
    "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1";
// C-11 (deferred, product decision): Cloudflare now also exposes a REST-style
// route alongside this `/compat` OpenAI-compatibility surface. We intentionally
// keep `/compat` as the default — flipping every existing CF user's endpoint
// blind would break live configs and needs the verified CF REST URL first. Do
// not change this value without that confirmation.
pub const DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL: &str =
    "https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/compat";
pub const DEFAULT_CLOUDFLARE_AI_GATEWAY_ID: &str = "default";
pub const DEFAULT_CLOUDFLARE_WORKERS_AI_MODEL: &str = "@cf/meta/llama-3.3-70b-instruct-fp8-fast";
pub const DEFAULT_CLOUDFLARE_AI_GATEWAY_MODEL: &str = "@cf/meta/llama-3.3-70b-instruct-fp8-fast";

/// Vertex AI's OpenAI-compatible chat completions endpoint lives behind a
/// regional URL that names the project. Returns the resolved base URL for a
/// `(project, location)` pair, ready for `/chat/completions` to be appended.
///
/// The `global` location lives at the bare host `aiplatform.googleapis.com`
/// (no `{location}-` prefix) because Google does not run a regional Anycast
/// frontend named `global`. Gemini 3.x is GA only via this location, so a
/// caller passing `location = "global"` builds the correct host instead of
/// a `https://global-aiplatform.googleapis.com/...` URL that DNS-fails.
/// Other locations (regional like `us-central1`, plus the continental
/// pseudo-regions `us`/`eu`) keep the historical `{location}-aiplatform`
/// shape so production deployments are unchanged.
pub fn vertex_base_url(project: &str, location: &str) -> String {
    let trimmed = location.trim();
    if trimmed.eq_ignore_ascii_case("global") {
        format!(
            "https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/endpoints/openapi"
        )
    } else {
        format!(
            "https://{trimmed}-aiplatform.googleapis.com/v1/projects/{project}/locations/{trimmed}/endpoints/openapi"
        )
    }
}

/// Resolve a bare-name model alias (e.g. `opus`, `sonnet`, `haiku`) to the
/// provider-preferred full model ID, so `squeezy --model opus` resolves to
/// `claude-opus-4-8` on Anthropic (or `claude-sonnet-4-6` for `sonnet`)
/// instead of being sent verbatim and
/// 404-ing downstream. Lookup is case-insensitive on the alias. Returns
/// `None` for inputs that don't match any alias, in which case callers
/// should pass the string through unchanged (it's presumed to be a full
/// model ID).
pub fn resolve_model_alias(provider: &str, alias: &str) -> Option<&'static str> {
    let normalized = alias.trim().to_ascii_lowercase();
    match (provider, normalized.as_str()) {
        ("anthropic", "opus" | "best" | "opus-4.8" | "opus-4-8") => Some("claude-opus-4-8"),
        ("anthropic", "opus-4.7" | "opus-4-7") => Some("claude-opus-4-7"),
        ("anthropic", "sonnet") => Some("claude-sonnet-4-6"),
        ("anthropic", "haiku") => Some("claude-haiku-4-5-20251001"),
        ("openai" | "azure_openai", "opus") => Some(DEFAULT_OPENAI_MODEL),
        ("openai" | "azure_openai", "sonnet") => Some("gpt-5.4-mini"),
        ("openai" | "azure_openai", "haiku") => Some("gpt-5.4-nano"),
        ("openai" | "azure_openai", "best") => Some(DEFAULT_OPENAI_MODEL),
        ("bedrock", "opus" | "best" | "opus-4.8" | "opus-4-8") => Some("anthropic.claude-opus-4-8"),
        ("bedrock", "sonnet") => Some(DEFAULT_BEDROCK_MODEL),
        ("bedrock", "haiku") => Some(BEDROCK_SMALL_FAST_MODEL),
        ("openrouter", "opus" | "best" | "opus-4.8" | "opus-4-8") => {
            Some("anthropic/claude-opus-4.8")
        }
        ("openrouter", "opus-4.7" | "opus-4-7") => Some("anthropic/claude-opus-4-7"),
        ("openrouter", "sonnet") => Some(DEFAULT_OPENROUTER_MODEL),
        ("vercel", "opus" | "best" | "opus-4.8" | "opus-4-8") => Some("anthropic/claude-opus-4.8"),
        ("vercel", "opus-4.7" | "opus-4-7") => Some("anthropic/claude-opus-4.7"),
        ("vercel", "sonnet") => Some(DEFAULT_VERCEL_AI_MODEL),
        ("portkey", "opus" | "best" | "opus-4.8" | "opus-4-8") => Some("anthropic/claude-opus-4-8"),
        ("portkey", "opus-4.7" | "opus-4-7") => Some("anthropic/claude-opus-4-7"),
        ("portkey", "sonnet") => Some(DEFAULT_PORTKEY_MODEL),
        ("cloudflare_workers_ai" | "cloudflare_ai_gateway", "opus-4.8" | "opus-4-8") => {
            Some("anthropic/claude-opus-4.8")
        }
        ("google", "opus" | "best") => Some(DEFAULT_GOOGLE_MODEL),
        ("google", "sonnet") => Some("gemini-3.5-flash"),
        ("google", "haiku") => Some("gemini-2.5-flash-lite"),
        _ => None,
    }
}

/// Cloudflare Workers AI's OpenAI-compatible chat completions endpoint is
/// per-account. Returns the resolved base URL for an `account_id`, ready for
/// `/chat/completions` to be appended. Built on top of the canonical
/// [`DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL`] template so callers that read
/// the template directly (e.g. config inspectors) and callers that resolve
/// it eagerly stay in sync.
pub fn cloudflare_workers_ai_base_url(account_id: &str) -> String {
    DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL.replace("{account_id}", account_id)
}

/// Cloudflare AI Gateway proxies any OpenAI-compatible upstream behind a
/// per-(account, gateway) URL. The `compat` suffix is the gateway's
/// OpenAI-format compatibility surface; underneath it can route to Workers AI,
/// OpenAI, Anthropic, etc. depending on the gateway's configured upstream.
pub fn cloudflare_ai_gateway_base_url(account_id: &str, gateway_id: &str) -> String {
    DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL
        .replace("{account_id}", account_id)
        .replace("{gateway_id}", gateway_id)
}

pub const MODEL_SELECTION_VERSION: u32 = 1;
pub const DEFAULT_EXA_MCP_URL: &str = "https://mcp.exa.ai/mcp";
pub const DEFAULT_EXA_API_KEY_ENV: &str = "EXA_API_KEY";
pub const DEFAULT_PARALLEL_MCP_URL: &str = "https://search.parallel.ai/mcp";
pub const DEFAULT_PARALLEL_API_KEY_ENV: &str = "PARALLEL_API_KEY";
pub const DEFAULT_WEBSEARCH_PROVIDER: &str = "exa";
pub const DEFAULT_MAX_OUTPUT_TOKENS: Option<u32> = None;
pub const DEFAULT_TOOL_SPILL_THRESHOLD_BYTES: usize = 25_000;
pub const DEFAULT_TOOL_PREVIEW_BYTES: usize = 2_000;
pub const DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND: usize = 50_000;
pub const DEFAULT_TOOL_OUTPUT_RETENTION_DAYS: u64 = 7;
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 16;
// Per-turn aggregate budgets across every tool the agent runs in a
// single turn. These defaults are sized so they never bind in realistic
// use; users who want strict cost caps can set tighter values in
// `squeezy.toml`. Kept finite (rather than `u64::MAX`) so the inspect
// output remains TOML-roundtrippable.
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: u64 = 10_000;
pub const DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN: u64 = 1_000_000_000;
pub const DEFAULT_MAX_SEARCH_FILES_PER_TURN: u64 = 1_000_000;
pub const DEFAULT_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_PROVIDER_REQUEST_MAX_RETRIES: u8 = 4;
pub const DEFAULT_PROVIDER_STREAM_MAX_RETRIES: u8 = 5;
pub const DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;
/// Default idle timeout (ms) for sockets parked in the shared HTTP
/// pool before reqwest evicts them. Matches reqwest's own default so
/// the centralized factory preserves pre-F08 per-provider behavior
/// when the user has not set a custom `[transport]` knob.
pub const DEFAULT_PROVIDER_POOL_IDLE_TIMEOUT_MS: u64 = 90_000;
/// Default cap on idle TCP connections kept per origin in the shared
/// HTTP pool. `u32::MAX` is effectively unbounded (mirrors reqwest's
/// `usize::MAX` default) so existing per-provider workloads keep
/// reusing as many warmed sockets as they did before the dispatcher
/// was centralized.
pub const DEFAULT_PROVIDER_POOL_MAX_IDLE_PER_HOST: u32 = u32::MAX;
/// Hard ceiling (ms) on any inter-retry sleep — including a
/// server-supplied `Retry-After` / `Retry-After-Ms` hint. Sized at one
/// minute so honest throttle hints (typically seconds) pass through
/// untouched while a malicious or buggy upstream can't park the agent
/// for hours by claiming a multi-day cooldown.
pub const DEFAULT_PROVIDER_MAX_RETRY_DELAY_MS: u64 = 60_000;
pub const DEFAULT_COST_WARN_PERCENT: u8 = 85;
// Per-turn model routing ("cheap-model fast path") defaults. The router
// inspects the user prompt before each turn's first LLM request; on a
// match it dispatches that turn to the provider's small-fast tier
// (`small_fast_model_for_provider`). The mid-turn escalation detector
// hands the same turn back to the parent model when the cheap model
// stalls. See `crates/squeezy-agent/src/turn_router.rs` for the
// classifier and the escalation signals; see chapter 11 of the
// cost-saving docs for the rationale.
pub const DEFAULT_ROUTING_ENABLED: bool = true;
/// The static, deterministic verb-heuristic fast-path. Independent of the
/// judge so users can disable one without the other.
pub const DEFAULT_ROUTING_HEURISTIC: bool = true;
pub const DEFAULT_ROUTING_LLM_JUDGE: bool = true;
/// Char-budget below which a turn is treated as a short follow-up ("ok",
/// "continue", "yes") and inherits the previous turn's routing decision
/// instead of paying for a judge call — a follow-up to a big parent turn
/// stays on the parent, a follow-up to a cheap turn stays cheap.
pub const DEFAULT_ROUTING_FOLLOW_UP_MAX_CHARS: u32 = 24;
pub const DEFAULT_ROUTING_CHEAP_ESCALATION_ERROR_THRESHOLD: u8 = 2;
pub const DEFAULT_ROUTING_ESCALATION_STICKY_TURNS: u8 = 3;
pub const DEFAULT_ROUTING_BYPASS_FOR_IMAGES: bool = true;
pub const DEFAULT_ROUTING_LARGE_ATTACHMENT_BYPASS_BYTES: u32 = 4_096;
/// Char-budget gate for the heuristic prefilter. Prompts longer than
/// this skip the slam-dunk path and fall through to the borderline
/// judge (or `Parent`). Sized at ~400 tokens of English at 5 chars/tok.
pub const DEFAULT_ROUTING_HEURISTIC_MAX_CHARS: u32 = 2_000;
/// Default for `[routing].linux_sandbox_sensitive_parent`: route prompts
/// mentioning Linux sandbox/container/kernel keywords to the parent model
/// even when the heuristic or judge would choose cheap.
pub const DEFAULT_ROUTING_LINUX_SANDBOX_SENSITIVE_PARENT: bool = true;
/// Char-budget gate for the LLM judge. Prompts longer than this skip the
/// judge call and route to `Parent` directly — long prompts almost
/// always carry the kind of nuance the cheap tier struggles with, and a
/// long judge call would erode the savings the router is trying to
/// produce. Sized at ~1500 tokens of English.
pub const DEFAULT_ROUTING_JUDGE_MAX_CHARS: u32 = 6_000;
// Per-subagent-invocation budgets, sized so each is only ever
// reached when something has demonstrably gone wrong (a stuck
// retry loop, a runaway scan, a model that won't emit a final
// answer). The natural exit path is the model emitting a final
// answer with no tool calls; legitimate research work has plenty of
// headroom under these numbers. Do NOT lower any of these in a
// "more conservative defaults" pass — that creates false-positive
// aborts on real workloads where the cost broker is already the
// load-bearing safeguard.
pub const DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL: u64 = 10_000;
pub const DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL: u64 = 1_000_000_000;
pub const DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL: u64 = 1_000_000;
/// Maximum number of subagents that may be active at once for a single
/// parent Agent. The registry rejects further `start()` calls until an
/// in-flight subagent finishes (lease drops). Keeps fanout flat and
/// predictable rather than letting a model spawn an unbounded swarm.
pub const DEFAULT_SUBAGENT_MAX_CONCURRENT: usize = 20;
// Last-resort belt on subagent model rounds. 1 000 rounds is well
// above what real long-running agent sessions reach — by then the
// `max_session_cost_usd_micros` broker has already capped the
// subagent (1 000 rounds at gpt-5.4-mini pricing is roughly $5–$10
// of spend, comfortably beyond the $5 default cap). Reaching this
// cap is a signal that the cost broker was either disabled or
// raised much higher than usual AND the model is failing to
// converge, both of which already indicate "something went wrong"
// — so it's safe to keep as a belt even when the user's principle
// rules out false-positive limits.
pub const DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS: usize = 1_000;
// Wall-clock ceiling for a single subagent run. Disabled by default
// (`0` = no cap). The earlier 300s default was a legacy carryover
// from when the subagent's event channel didn't heartbeat back to
// the parent: a subagent could go silent from the parent's
// perspective for >60s, the parent's per-event timeout would fire,
// and the parent would abort the turn while the subagent was still
// legitimately working. That root cause is fixed now (the
// subagent's `ToolProgress` heartbeats forward to the parent's tx,
// so the parent's window resets on every subagent tool tick), so we
// no longer need a wall-clock fallback to compensate. A subagent
// doing real research work — `explore` walking a 50-file callgraph,
// `delegate` reasoning through a multi-step plan — is no different
// from the main agent's right to run for as long as the user is
// willing to pay. The load-bearing safeguards are the cost broker
// (`max_session_cost_usd_micros`), the agent-side cancellation
// token, and the provider's own rate limits / connection timeouts.
// Re-enable with `max_runtime_secs = <secs>` in TOML or
// `SQUEEZY_SUBAGENT_MAX_RUNTIME_SECS=<secs>` for environments that
// want a wall-clock belt.
pub const DEFAULT_SUBAGENT_MAX_RUNTIME_SECS: u64 = 0;
// Generous default sized for Plan/Delegate/Review summaries under a
// reasoning model: thinking tokens burn first, then the actual summary.
// 64K leaves room for both across every model we ship a preset for. The
// OpenAI Responses API surfaces an under-budget run as
// `response.incomplete: max_output_tokens` (a hard error in our SSE parser),
// so the failure mode of being too tight is loud, not silent. Used only
// as a fallback when the parent agent has no explicit max_output_tokens.
pub const DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS: u32 = 64_000;
/// Floor for the DocHelp subagent's output budget when the parent does not
/// cap `max_output_tokens`. DocHelp's "summary" is the user-visible answer
/// (not a synopsis of a tool-driven exploration), so it gets a much higher
/// floor than other subagent kinds.
pub const DEFAULT_DOC_HELP_MAX_OUTPUT_TOKENS: u32 = 32_000;
pub const DEFAULT_TICK_RATE_MS: u64 = 50;
pub const DEFAULT_TELEMETRY_ENDPOINT: &str =
    "https://squeezy-telemetry.esqueezy.workers.dev/v1/batch";
pub const DEFAULT_FEEDBACK_ENDPOINT: &str =
    "https://squeezy-telemetry.esqueezy.workers.dev/v1/feedback";
pub const DEFAULT_REPORT_ENDPOINT: &str =
    "https://squeezy-telemetry.esqueezy.workers.dev/v1/report";
pub const DEFAULT_FEEDBACK_MAX_BYTES: usize = 16 * 1024;
pub const DEFAULT_REPORT_MAX_BYTES: usize = 2 * 1024 * 1024;
pub const PROJECT_SETTINGS_FILE: &str = "squeezy.toml";
pub const DEFAULT_SQUEEZY_SKILLS_DIR: &str = ".squeezy/skills";
pub const DEFAULT_SESSION_LOG_RETENTION_DAYS: u64 = 30;
/// Days an archived session lives before the retention sweep permanently
/// deletes it. Live sessions hit `log_retention_days` first, then move to
/// `archived/<id>/`, where they linger for this long. Matches the live
/// default so an idle workspace keeps roughly 60 days of recoverable
/// history (30 live + 30 archived) without manual intervention.
pub const DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS: u64 = 30;
pub const DEFAULT_SESSION_MAX_EVENT_BYTES: usize = 131_072;
pub const DEFAULT_SESSION_MAX_SESSION_BYTES: usize = 52_428_800;
pub const DEFAULT_CONTEXT_ATTACHMENT_MAX_BYTES: usize = 1_048_576;
// Window used for the percent-of-window compaction thresholds when the
// active model's real context window is unknown (no registry entry and none
// set in `squeezy.toml`). Every compaction threshold is a fraction of the
// resolved window; this value only stands in when that window cannot be
// determined. 128k matches the smallest window common to modern models.
pub const DEFAULT_CONTEXT_FALLBACK_WINDOW_TOKENS: u64 = 128_000;
/// Percent of the raw window treated as usable when no per-model curated value
/// or explicit override is set. Mirrors the limit resolver's
/// `default_effective_context_window_percent` so compaction and request sizing
/// agree on the usable budget.
pub const DEFAULT_CONTEXT_EFFECTIVE_WINDOW_PERCENT: u8 = 95;
/// Flat token reserve carved off the effective window for system framing when
/// no override is set. Mirrors `squeezy_llm::DEFAULT_BASELINE_RESERVE_TOKENS`.
pub const DEFAULT_CONTEXT_BASELINE_RESERVE_TOKENS: u64 = 12_000;
pub const DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS: usize = 16;
pub const DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS: usize = 10;
pub const DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES: usize = 12_000;
pub const DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES: usize = 32_768;
pub const DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES: usize = 16_384;
/// Deprecated: the legacy summarize/mid-turn threshold percent. Retained only
/// as the default for the parsed-but-unused `threshold_percent` config field so
/// older configs keep loading; no longer drives compaction (summarize now fires
/// at the effective window).
pub const DEFAULT_CONTEXT_COMPACTION_THRESHOLD_PERCENT: u8 = 80;
/// Fraction of the resolved context window (out of 100) at which the
/// pre-summarize nudge fires. Sits below the summarize threshold so the user
/// is warned, and can `/pin`, before any lossy summarize runs.
pub const DEFAULT_CONTEXT_WARN_AT_PERCENT: u8 = 85;
/// Fraction of the resolved context window (out of 100) above which the
/// post-turn summarize gate bypasses the `min_items` floor. A conversation of
/// a few but enormous items (e.g. several large `read_file` pairs) can dwarf
/// the window while sitting under `min_items`; once it crosses this high-water
/// mark it is summarized regardless of item count.
pub const HIGH_WATER_BYPASS_PCT: u64 = 90;
/// Max output tokens to request when the model-assisted compaction strategy
/// is active.
pub const DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS: u32 = 1_500;
/// Timeout for a single model-assisted compaction round-trip. On expiry the
/// pipeline falls back to the extractive summary.
pub const DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS: u64 = 30;
/// When strategy = LayeredFallback, model-assist only kicks in once the
/// dropped slice exceeds this many tokens; smaller slices stay extractive.
pub const DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS: u32 = 4_000;
/// Fraction of the resolved context window (out of 100) at which the trim
/// (micro) tier fires. Set low: trimming only clears older bulky tool-output
/// bodies in place (structure preserved, no summary), so it runs early and
/// keeps the working set lean long before the lossy summarize tier.
pub const DEFAULT_CONTEXT_TRIM_AT_PERCENT: u8 = 40;
/// Keep this many newest compactable tool results verbatim during
/// micro-compaction; older results are rewritten to a placeholder. 5
/// is enough to resolve typical "what did the last read of foo.rs
/// show?" follow-ups without forcing a re-read.
pub const DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT: usize = 5;
pub const DEFAULT_AGENT_COMPAT_SKILLS_DIR: &str = ".agents/skills";
/// Tools whose full JSON schema is always sent up-front in every request,
/// independent of `[tools].lazy_schema_loading`.
///
/// These are the cheap-and-likely-needed-every-turn tools: bounded file
/// reads/writes, structured patching, search, shell, and graph-backed navigation. Heavyweight
/// or rarely-used tools (e.g. `verify`, `webfetch`, `websearch`) are
/// intentionally **not** in this list so they only cost prompt bytes once
/// the model explicitly attaches them via `load_tool_schema`.
///
/// `load_tool_schema` is not duplicated here on purpose: it is forced into the
/// request `tools` array by name in `squeezy_agent::request_tool_specs`, and
/// `squeezy_agent::tool_is_core_schema` treats it as always-core. Listing it
/// in two places risks future skew if one site is updated without the other.
///
/// `update_task_state` is intentionally omitted from model-visible schemas.
/// The runtime derives visible progress from turn/tool lifecycle events.
pub const DEFAULT_CORE_TOOL_NAMES: &[&str] = &[
    "glob",
    "grep",
    "read_file",
    "read_tool_output",
    "write_file",
    "apply_patch",
    "shell",
    "decl_search",
    "definition_search",
    "diff_context",
    "downstream_flow",
    "hierarchy",
    "plan_patch",
    "read_slice",
    "reference_search",
    "repo_map",
    "symbol_context",
    "upstream_flow",
];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub provider: ProviderConfig,
    pub model: String,
    /// Cheap model id used for low-stakes background calls: compaction
    /// summary, AI reviewer classifier, auto-approver. `None` falls back to
    /// `small_fast_model_for_provider(provider)`, then to `model` if the
    /// provider has no curated cheap tier (e.g. Ollama). Configured via
    /// `[model].small_fast_model` in TOML or `SQUEEZY_SMALL_FAST_MODEL`.
    pub small_fast_model: Option<String>,
    pub profile: ModelProfile,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub instructions: String,
    pub max_output_tokens: Option<u32>,
    /// Sampling temperature. `None` leaves the provider/model default in
    /// place. Configured via `[model].temperature`.
    pub temperature: Option<f32>,
    /// Nucleus-sampling cutoff. `None` leaves the provider/model default in
    /// place. Configured via `[model].top_p`.
    pub top_p: Option<f32>,
    /// Deterministic sampling seed where supported. `None` leaves generation
    /// non-deterministic. Configured via `[model].seed`.
    pub seed: Option<u64>,
    /// Stop sequences. Empty leaves the provider/model default in place.
    /// Configured via `[model].stop`.
    pub stop: Vec<String>,
    /// OpenAI-style frequency penalty where supported. `None` leaves the
    /// provider/model default in place. Configured via
    /// `[model].frequency_penalty`.
    pub frequency_penalty: Option<f32>,
    /// OpenAI-style presence penalty where supported. `None` leaves the
    /// provider/model default in place. Configured via
    /// `[model].presence_penalty`.
    pub presence_penalty: Option<f32>,
    /// Forwarded as `tool_choice` to providers when tools are advertised.
    /// `None` omits the field; providers default to `auto`. Set to
    /// `"required"` to force a tool call every turn — needed for
    /// chat-completions models that ignore `auto` (Qwen via OpenRouter,
    /// smaller MoEs). Configured via `[model].tool_choice` in TOML or
    /// `SQUEEZY_TOOL_CHOICE` env var.
    pub tool_choice: Option<String>,
    /// Forwarded to the provider as `parallel_tool_calls` on the main
    /// agent (and subagent) request. `None` (the default) omits the
    /// field, leaving the provider's default — *parallel* for OpenAI
    /// Responses / Chat-Completions — in place, so the model is already
    /// free to batch independent tool calls without re-sending the
    /// growing prefix on extra rounds. `Some(true)` forwards an explicit
    /// opt-in; `Some(false)` forces serial tool calls. Configured via
    /// `[model].parallel_tool_calls` or `SQUEEZY_PARALLEL_TOOL_CALLS`.
    pub parallel_tool_calls: Option<bool>,
    /// When `true`, append a short system-prompt nudge encouraging the
    /// model to batch independent read-only lookups into one assistant
    /// turn. `false` (the default) leaves the prompt byte-for-byte
    /// unchanged. Configured via `[model].batch_tool_calls_hint` or
    /// `SQUEEZY_BATCH_TOOL_CALLS_HINT`.
    pub batch_tool_calls_hint: bool,
    pub stream_idle_timeout: Duration,
    pub tick_rate: Duration,
    pub workspace_root: PathBuf,
    pub permissions: PermissionPolicy,
    pub session_mode: SessionMode,
    #[serde(default)]
    pub session_resume_picker: SessionResumePicker,
    pub session_logs: SessionLogConfig,
    pub context_compaction: ContextCompactionConfig,
    pub subagents: SubagentConfig,
    pub store_responses: bool,
    pub exploration_graph: bool,
    pub max_parallel_tools: usize,
    pub tool_spill_threshold_bytes: usize,
    pub tool_preview_bytes: usize,
    pub max_tool_result_bytes_per_round: usize,
    pub tool_output_retention_days: u64,
    pub exa_mcp_url: String,
    pub exa_api_key_env: String,
    pub parallel_mcp_url: String,
    pub parallel_api_key_env: String,
    /// Websearch backend identifier; expected values are "exa" or "parallel".
    /// Stored as a string so squeezy-core has no dependency on squeezy-tools.
    pub websearch_provider: String,
    pub max_tool_calls_per_turn: u64,
    pub max_tool_bytes_read_per_turn: u64,
    pub max_search_files_per_turn: u64,
    pub max_session_cost_usd_micros: Option<u64>,
    pub cost_warn_percent: u8,
    /// Suppress all prompt caching for this session (env
    /// `SQUEEZY_DISABLE_PROMPT_CACHE`). Threads into every `LlmRequest` so
    /// each turn is billed at full input price with no cache_control markers
    /// and an OpenAI cache-busting nonce — used for deterministic,
    /// cache-independent cost comparisons.
    pub disable_prompt_cache: bool,
    /// Optional pre-flight ceiling on the estimated input tokens of a single
    /// LLM round. `None` (the default) disables the gate entirely, leaving
    /// every round dispatched exactly as before. When set, the agent
    /// estimates the assembled request's input tokens before sending; if the
    /// estimate exceeds this value it first attempts mid-turn compaction and,
    /// if the round is still over, gates the dispatch with a clear status
    /// instead of paying for an oversized round.
    pub max_round_input_tokens: Option<u64>,
    pub routing: RoutingConfig,
    pub telemetry: TelemetryConfig,
    pub feedback: FeedbackConfig,
    pub redaction: RedactionConfig,
    pub skills: SkillsConfig,
    pub graph: GraphConfig,
    pub cache: CacheConfig,
    pub tools: ToolSchemaConfig,
    pub checkpoints_enabled: bool,
    pub tui: TuiConfig,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    /// Raw per-provider settings (`[providers.<name>]`), retained so the
    /// use-site routing resolvers and the config screen can read/edit
    /// per-provider overrides (reroute/judge models, judge prompt, reroute
    /// allowlist) without rebuilding the whole config. Routing is
    /// provider-scoped and never crosses providers.
    pub providers: BTreeMap<String, ProviderSettings>,
    /// Per-model context-window overrides keyed by `"<provider>:<model>"`
    /// (`[model_limits."openai:gpt-5.5"]`). The single user-facing knob for the
    /// limit resolver; an entry for the active model wins over every
    /// auto-resolved layer. Keyed per-model so switching models never carries a
    /// stale window forward.
    #[serde(default)]
    pub model_limits: BTreeMap<String, ModelLimitOverride>,
    pub hardening: HardeningConfig,
    pub config_sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_warnings: Vec<ConfigWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigWarning {
    pub source: String,
    pub field: String,
}

/// Canonical provider slug (`openai`, `anthropic`, an aggregator preset, …)
/// used both as the `[providers.<slug>]` key and as the provider half of a
/// `[model_limits."<provider>:<model>"]` key. One source of truth so the config
/// screen's write key and every reader of [`AppConfig::model_limit_key`] agree.
pub fn provider_slug(provider: &ProviderConfig) -> &'static str {
    match provider {
        ProviderConfig::OpenAi(_) => "openai",
        ProviderConfig::Anthropic(_) => "anthropic",
        ProviderConfig::Google(_) => "google",
        ProviderConfig::AzureOpenAi(_) => "azure_openai",
        ProviderConfig::Bedrock(_) => "bedrock",
        ProviderConfig::Ollama(_) => "ollama",
        ProviderConfig::OpenAiCodex(_) => "openai_codex",
        ProviderConfig::GitHubCopilot(_) => "github_copilot",
        ProviderConfig::OpenAiCompatible(config) => config.preset.as_str(),
        ProviderConfig::Faux(_) => "faux",
    }
}

impl AppConfig {
    /// The `[model_limits]` key for the active `(provider, model)`.
    pub fn model_limit_key(&self) -> String {
        format!("{}:{}", provider_slug(&self.provider), self.model)
    }

    pub fn from_env() -> Self {
        Self::from_env_vars(None, |name| env::var(name).ok())
    }

    pub fn from_env_and_settings() -> Result<Self> {
        Self::from_default_paths_and_env_with_provider_value(None, None)
    }

    pub fn from_env_and_settings_with_provider(provider: &str) -> Result<Self> {
        Self::from_default_paths_and_env_with_provider_value(Some(provider), None)
    }

    /// Like `from_env_and_settings`, but applies the named TOML profile
    /// (`[profiles.<name>]`) on top of the merged base settings before the
    /// env-var layer is applied. Errors if the profile is not configured.
    pub fn from_env_and_settings_with_profile(
        provider: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Self> {
        Self::from_default_paths_and_env_with_provider_value(provider, profile)
    }

    pub fn from_settings_path_and_env(path: PathBuf) -> Result<Self> {
        let (settings, sources, warnings) = SettingsFile::load_optional_source(&path, "settings")?;
        Self::try_from_settings_and_env_vars_with_sources_and_warnings(
            settings,
            sources,
            warnings,
            None,
            |name| env::var(name).ok(),
        )
    }

    pub fn from_settings_path_and_env_with_provider(path: PathBuf, provider: &str) -> Result<Self> {
        let (settings, sources, warnings) = SettingsFile::load_optional_source(&path, "settings")?;
        Self::try_from_settings_and_env_vars_with_sources_and_warnings(
            settings,
            sources,
            warnings,
            Some(provider),
            |name| env::var(name).ok(),
        )
    }

    pub fn from_env_with_provider(provider: &str) -> Self {
        Self::from_env_vars(Some(provider), |name| env::var(name).ok())
    }

    fn from_env_vars(
        cli_provider: Option<&str>,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self::try_from_settings_and_env_vars(SettingsFile::default(), cli_provider, &mut var)
            .unwrap_or_else(|error| {
                // Surfaces in real runs through tracing; tests have no subscriber
                // so they fall back silently the way they always did.
                tracing::warn!(
                    target: "squeezy_core::config",
                    %error,
                    "config resolution failed; falling back to built-in defaults",
                );
                Self::built_in_defaults()
            })
    }

    #[cfg(test)]
    fn from_settings_and_env_vars(
        settings: SettingsFile,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self::try_from_settings_and_env_vars(settings, None, &mut var)
            .unwrap_or_else(|_| Self::built_in_defaults())
    }

    fn try_from_settings_and_env_vars(
        settings: SettingsFile,
        cli_provider: Option<&str>,
        var: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        Self::try_from_settings_and_env_vars_with_sources(
            settings,
            vec!["defaults".to_string()],
            cli_provider,
            var,
        )
    }

    fn try_from_settings_and_env_vars_with_sources(
        settings: SettingsFile,
        sources: Vec<String>,
        cli_provider: Option<&str>,
        var: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        Self::try_from_settings_and_env_vars_with_sources_and_warnings(
            settings,
            sources,
            Vec::new(),
            cli_provider,
            var,
        )
    }

    fn try_from_settings_and_env_vars_with_sources_and_warnings(
        settings: SettingsFile,
        mut sources: Vec<String>,
        mut config_warnings: Vec<ConfigWarning>,
        cli_provider: Option<&str>,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        let workspace_root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut env_used = false;
        let mut get_var = |name: &str| {
            let value = var(name);
            if value.is_some() {
                env_used = true;
            }
            value
        };

        let model_settings = settings.model_settings.clone().unwrap_or_default();
        let env_provider = get_var("SQUEEZY_PROVIDER");
        let provider_name = cli_provider
            .map(str::to_string)
            .or(env_provider)
            .or(model_settings.provider)
            .or(settings.provider.clone())
            .unwrap_or_else(|| "openai".to_string())
            .trim()
            .to_ascii_lowercase();
        let providers = settings.providers.unwrap_or_default();
        let model_limits = settings.model_limits.unwrap_or_default();
        let provider = match provider_name.as_str() {
            "anthropic" | "claude" => ProviderConfig::Anthropic(AnthropicConfig {
                api_key_env: get_var("ANTHROPIC_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "anthropic", "api_key_env"))
                    .unwrap_or_else(|| "SQUEEZY_ANTHROPIC_KEY".to_string()),
                api_key: provider_setting(&providers, "anthropic", "api_key"),
                base_url: get_var("ANTHROPIC_BASE_URL")
                    .or_else(|| provider_setting(&providers, "anthropic", "base_url"))
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_BASE_URL.to_string()),
                transport: provider_transport_settings(&providers, &["anthropic"]),
            }),
            "google" | "gemini" => ProviderConfig::Google(GoogleConfig {
                api_key_env: get_var("GOOGLE_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "google", "api_key_env"))
                    .unwrap_or_else(|| "SQUEEZY_GOOGLE_KEY".to_string()),
                api_key: provider_setting(&providers, "google", "api_key"),
                base_url: get_var("GOOGLE_BASE_URL")
                    .or_else(|| provider_setting(&providers, "google", "base_url"))
                    .unwrap_or_else(|| DEFAULT_GOOGLE_BASE_URL.to_string()),
                transport: provider_transport_settings(&providers, &["google"]),
            }),
            "azure" | "azure-openai" | "azure_openai" => {
                // Entra ID opt-in is layered: either an explicit
                // `use_entra_id = true` in TOML, or the operator pre-populated
                // `AZURE_OPENAI_BEARER_TOKEN` and wants squeezy to honor it
                // without separately flipping the bool. The presence of a
                // bearer in the env is sufficient signal: callers that
                // truly want the api-key path leave that env var unset.
                let entra_bearer_token = get_var("AZURE_OPENAI_BEARER_TOKEN")
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty());
                let use_entra_id = provider_setting_bool_any(
                    &providers,
                    &["azure_openai", "azure"],
                    "use_entra_id",
                )
                .unwrap_or(false)
                    || entra_bearer_token.is_some();
                ProviderConfig::AzureOpenAi(AzureOpenAiConfig {
                    api_key_env: get_var("AZURE_OPENAI_API_KEY_ENV")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_key_env"))
                        .or_else(|| provider_setting(&providers, "azure", "api_key_env"))
                        .unwrap_or_else(|| "SQUEEZY_AZURE_OPENAI_KEY".to_string()),
                    api_key: provider_setting(&providers, "azure_openai", "api_key")
                        .or_else(|| provider_setting(&providers, "azure", "api_key")),
                    base_url: get_var("AZURE_OPENAI_BASE_URL")
                        .or_else(|| provider_setting(&providers, "azure_openai", "base_url"))
                        .or_else(|| provider_setting(&providers, "azure", "base_url"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_BASE_URL.to_string()),
                    api_version: get_var("AZURE_OPENAI_API_VERSION")
                        .or_else(|| provider_setting(&providers, "azure_openai", "api_version"))
                        .or_else(|| provider_setting(&providers, "azure", "api_version"))
                        .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_API_VERSION.to_string()),
                    deployment_name_map: provider_setting_deployment_name_map(
                        &providers,
                        &["azure_openai", "azure"],
                    ),
                    extra_headers: provider_setting_headers_any(
                        &providers,
                        &["azure_openai", "azure"],
                    )
                    .unwrap_or_default(),
                    use_entra_id,
                    entra_bearer_token,
                    transport: provider_transport_settings(&providers, &["azure_openai", "azure"]),
                })
            }
            "bedrock" | "amazon-bedrock" | "amazon_bedrock" => {
                ProviderConfig::Bedrock(BedrockConfig {
                    region: get_var("AWS_REGION")
                        .or_else(|| get_var("AWS_DEFAULT_REGION"))
                        .or_else(|| provider_setting(&providers, "bedrock", "region"))
                        .unwrap_or_else(|| DEFAULT_BEDROCK_REGION.to_string()),
                    base_url: get_var("BEDROCK_BASE_URL")
                        .or_else(|| provider_setting(&providers, "bedrock", "base_url")),
                    // Pick up `AWS_BEARER_TOKEN_BEDROCK` exactly like
                    // boto3 / aws-sdk-js do; an empty string is treated
                    // as "unset" so a shell that exports the var but
                    // leaves it blank falls through to the default
                    // credential chain instead of failing with
                    // "empty bearer token".
                    bearer_token: get_var("AWS_BEARER_TOKEN_BEDROCK")
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty()),
                    request_metadata: provider_setting_request_metadata(&providers, "bedrock")
                        .unwrap_or_default(),
                    transport: provider_transport_settings(&providers, &["bedrock"]),
                })
            }
            "ollama" | "local" => ProviderConfig::Ollama(OllamaConfig {
                base_url: get_var("OLLAMA_BASE_URL")
                    .or_else(|| provider_setting(&providers, "ollama", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OLLAMA_BASE_URL.to_string()),
                route_style: get_var("OLLAMA_ROUTE_STYLE")
                    .or_else(|| provider_setting(&providers, "ollama", "route_style"))
                    .as_deref()
                    .and_then(OllamaRoute::parse)
                    .unwrap_or_default(),
                transport: provider_transport_settings(&providers, &["ollama"]),
            }),
            "openai" => ProviderConfig::OpenAi(OpenAiConfig {
                api_key_env: get_var("OPENAI_API_KEY_ENV")
                    .or_else(|| provider_setting(&providers, "openai", "api_key_env"))
                    .unwrap_or_else(|| "SQUEEZY_OPENAI_KEY".to_string()),
                api_key: provider_setting(&providers, "openai", "api_key"),
                base_url: get_var("OPENAI_BASE_URL")
                    .or_else(|| provider_setting(&providers, "openai", "base_url"))
                    .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string()),
                // OPENAI_ORG_ID is the canonical env name in OpenAI's own
                // SDKs; OPENAI_ORGANIZATION is the long-form alias they
                // accept too. Honor both so users porting from other
                // tooling don't get surprised.
                organization: get_var("OPENAI_ORG_ID")
                    .or_else(|| get_var("OPENAI_ORGANIZATION"))
                    .or_else(|| provider_setting(&providers, "openai", "organization"))
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                project: get_var("OPENAI_PROJECT_ID")
                    .or_else(|| get_var("OPENAI_PROJECT"))
                    .or_else(|| provider_setting(&providers, "openai", "project"))
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                service_tier: get_var("OPENAI_SERVICE_TIER")
                    .or_else(|| provider_setting(&providers, "openai", "service_tier"))
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                transport: provider_transport_settings(&providers, &["openai"]),
            }),
            "openai-codex" | "openai_codex" | "chatgpt" => {
                ProviderConfig::OpenAiCodex(OpenAiCodexConfig {
                    base_url: get_var("OPENAI_CODEX_BASE_URL")
                        .or_else(|| provider_setting(&providers, "openai_codex", "base_url"))
                        .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_BASE_URL.to_string()),
                    originator: get_var("OPENAI_CODEX_ORIGINATOR")
                        .or_else(|| provider_setting(&providers, "openai_codex", "originator"))
                        .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_ORIGINATOR.to_string()),
                    transport: provider_transport_settings(&providers, &["openai_codex"]),
                })
            }
            "github-copilot" | "github_copilot" | "copilot" => {
                ProviderConfig::GitHubCopilot(GitHubCopilotConfig {
                    transport: provider_transport_settings(
                        &providers,
                        &["github_copilot", "github-copilot", "copilot"],
                    ),
                })
            }
            "faux" | "mock" => ProviderConfig::Faux(FauxConfig {
                script: get_var("SQUEEZY_FAUX_SCRIPT")
                    .or_else(|| provider_setting(&providers, "faux", "script")),
                name: None,
                transport: provider_transport_settings(&providers, &["faux"]),
            }),
            other if OpenAiCompatiblePreset::parse(other).is_some() => {
                let preset =
                    OpenAiCompatiblePreset::parse(other).expect("guarded by match condition");
                build_openai_compatible_config(preset, &providers, &mut get_var)?
            }
            unknown => {
                return Err(SqueezyError::Config(format!(
                    "model.provider: unknown provider {unknown:?}"
                )));
            }
        };
        validate_provider_base_urls(&provider)?;
        let default_model = match &provider {
            ProviderConfig::OpenAi(_) => provider_setting(&providers, "openai", "default_model")
                .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string()),
            ProviderConfig::Anthropic(_) => {
                provider_setting(&providers, "anthropic", "default_model")
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string())
            }
            ProviderConfig::Google(_) => provider_setting(&providers, "google", "default_model")
                .unwrap_or_else(|| DEFAULT_GOOGLE_MODEL.to_string()),
            ProviderConfig::AzureOpenAi(_) => {
                provider_setting(&providers, "azure_openai", "default_model")
                    .or_else(|| provider_setting(&providers, "azure", "default_model"))
                    .unwrap_or_else(|| DEFAULT_AZURE_OPENAI_MODEL.to_string())
            }
            ProviderConfig::Bedrock(_) => provider_setting(&providers, "bedrock", "default_model")
                .unwrap_or_else(|| DEFAULT_BEDROCK_MODEL.to_string()),
            ProviderConfig::Ollama(_) => provider_setting(&providers, "ollama", "default_model")
                .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string()),
            ProviderConfig::OpenAiCodex(_) => {
                provider_setting(&providers, "openai_codex", "default_model")
                    .unwrap_or_else(|| DEFAULT_OPENAI_CODEX_MODEL.to_string())
            }
            ProviderConfig::GitHubCopilot(_) => {
                provider_setting(&providers, "github_copilot", "default_model")
                    .or_else(|| provider_setting(&providers, "github-copilot", "default_model"))
                    .or_else(|| provider_setting(&providers, "copilot", "default_model"))
                    .unwrap_or_else(|| DEFAULT_GITHUB_COPILOT_MODEL.to_string())
            }
            ProviderConfig::OpenAiCompatible(config) => {
                provider_setting(&providers, config.preset.as_str(), "default_model")
                    .unwrap_or_else(|| config.preset.default_model().to_string())
            }
            ProviderConfig::Faux(_) => provider_setting(&providers, "faux", "default_model")
                .unwrap_or_else(|| DEFAULT_FAUX_MODEL.to_string()),
        };
        let profile = get_var("SQUEEZY_PROFILE")
            .or(model_settings.profile)
            .or(settings.profile)
            .as_deref()
            .and_then(ModelProfile::parse)
            .unwrap_or_default();
        let raw_model = get_var("SQUEEZY_MODEL")
            .or(model_settings.model)
            .or(settings.model)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(default_model);
        let provider_slug = match &provider {
            ProviderConfig::OpenAi(_) => "openai",
            ProviderConfig::Anthropic(_) => "anthropic",
            ProviderConfig::Google(_) => "google",
            ProviderConfig::AzureOpenAi(_) => "azure_openai",
            ProviderConfig::Bedrock(_) => "bedrock",
            ProviderConfig::Ollama(_) => "ollama",
            ProviderConfig::OpenAiCodex(_) => "openai_codex",
            ProviderConfig::GitHubCopilot(_) => "github_copilot",
            ProviderConfig::OpenAiCompatible(_) => "",
            ProviderConfig::Faux(_) => "faux",
        };
        // Legacy GLOBAL reroute-model override (env → `[model].small_fast_model`).
        // The per-provider override (`[providers.<slug>].cheap_model`) and the
        // built-in per-provider default are layered on top at the use-site
        // `cheap_model_for` in squeezy-agent, which reads `AppConfig.providers`.
        let small_fast_model = get_var("SQUEEZY_SMALL_FAST_MODEL")
            .or(model_settings.small_fast_model.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let model = resolve_model_alias(provider_slug, &raw_model)
            .map(str::to_string)
            .unwrap_or(raw_model);
        let reasoning_effort = model_settings.reasoning_effort;
        let max_output_tokens = get_var("SQUEEZY_MAX_OUTPUT_TOKENS")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|value| *value > 0)
            .or(model_settings.max_output_tokens)
            .or(DEFAULT_MAX_OUTPUT_TOKENS);
        let temperature = model_settings.temperature;
        let top_p = model_settings.top_p;
        let seed = model_settings.seed;
        let stop = model_settings.stop.unwrap_or_default();
        let frequency_penalty = model_settings.frequency_penalty;
        let presence_penalty = model_settings.presence_penalty;
        add_provider_model_warnings(
            &provider,
            &model,
            ModelSamplingOptions {
                temperature,
                top_p,
                seed,
                stop_set: !stop.is_empty(),
                frequency_penalty,
                presence_penalty,
            },
            max_output_tokens,
            &mut config_warnings,
        );
        // M-64: the `Custom` preset carries no URL allow-list and accepts
        // whatever `base_url` the (possibly project-local) config supplies,
        // making it a credential-exfil primitive. `build_openai_compatible_config`
        // already emits a `tracing::warn!`, but mirror the M-58 path and also
        // push a structured `ConfigWarning` so the warning reaches the
        // operator-facing channel even when tracing is unconfigured.
        if let ProviderConfig::OpenAiCompatible(compatible) = &provider
            && compatible.preset == OpenAiCompatiblePreset::Custom
        {
            config_warnings.push(ConfigWarning {
                source: "providers.custom".to_string(),
                field: format!(
                    "Custom preset bypasses the URL allow-list; verify base_url={} \
                     is trusted before credentials are sent to it.",
                    compatible.base_url
                ),
            });
        }
        let tool_choice = get_var("SQUEEZY_TOOL_CHOICE")
            .map(|raw| raw.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(model_settings.tool_choice.clone());
        // Tri-state: env override wins; otherwise fall back to the TOML
        // `[model]` value; `None` leaves the provider default in place.
        let parallel_tool_calls = parse_tristate_bool(get_var("SQUEEZY_PARALLEL_TOOL_CALLS"))
            .or(model_settings.parallel_tool_calls);
        let batch_tool_calls_hint = parse_tristate_bool(get_var("SQUEEZY_BATCH_TOOL_CALLS_HINT"))
            .or(model_settings.batch_tool_calls_hint)
            .unwrap_or(false);
        let provider_timeout_keys = provider_settings_keys(&provider);
        let stream_idle_timeout_ms = parse_u64(
            get_var("SQUEEZY_STREAM_IDLE_TIMEOUT_MS")
                .or_else(|| {
                    model_settings
                        .stream_idle_timeout_ms
                        .map(|value| value.to_string())
                })
                .or_else(|| {
                    provider_u64_setting_any(
                        &providers,
                        provider_timeout_keys,
                        "stream_idle_timeout_ms",
                    )
                }),
            DEFAULT_STREAM_IDLE_TIMEOUT_MS,
        );
        let web = settings.web.unwrap_or_default();
        let exa_mcp_url = get_var("SQUEEZY_EXA_MCP_URL")
            .or(web.exa_mcp_url)
            .unwrap_or_else(|| DEFAULT_EXA_MCP_URL.to_string());
        let exa_api_key_env = get_var("SQUEEZY_EXA_API_KEY_ENV")
            .or(web.exa_api_key_env)
            .unwrap_or_else(|| DEFAULT_EXA_API_KEY_ENV.to_string());
        let parallel_mcp_url = get_var("SQUEEZY_PARALLEL_MCP_URL")
            .or(web.parallel_mcp_url)
            .unwrap_or_else(|| DEFAULT_PARALLEL_MCP_URL.to_string());
        let parallel_api_key_env = get_var("SQUEEZY_PARALLEL_API_KEY_ENV")
            .or(web.parallel_api_key_env)
            .unwrap_or_else(|| DEFAULT_PARALLEL_API_KEY_ENV.to_string());
        let websearch_provider = get_var("SQUEEZY_WEBSEARCH_PROVIDER")
            .or(web.websearch_provider)
            .unwrap_or_else(|| DEFAULT_WEBSEARCH_PROVIDER.to_string());
        let requested_store_responses = get_var("SQUEEZY_STORE_RESPONSES")
            .as_deref()
            .map(parse_enabled_bool)
            .unwrap_or(model_settings.store_responses.unwrap_or(false));
        let store_responses = requested_store_responses
            && matches!(
                provider,
                ProviderConfig::OpenAi(_) | ProviderConfig::AzureOpenAi(_)
            );
        let agent_settings = settings.agent.unwrap_or_default();
        // Exploration graph prefetch defaults to on, and the documented env-var
        // override is `SQUEEZY_EXPLORATION_GRAPH=off|false|...`. Treating
        // the variable as a disable-only override keeps the documented values
        // working without silently flipping the default off on typos or empty
        // strings, matching how `SQUEEZY_TELEMETRY` and `SQUEEZY_FEEDBACK`
        // handle their own default-on flags.
        let settings_exploration_graph = agent_settings.exploration_graph.unwrap_or(true);
        let exploration_graph_var = get_var("SQUEEZY_EXPLORATION_GRAPH");
        let exploration_graph = if parse_disabled_bool(exploration_graph_var.as_deref()) {
            false
        } else {
            settings_exploration_graph
        };
        let budgets = settings.budgets.unwrap_or_default();
        let max_parallel_tools = get_var("SQUEEZY_MAX_PARALLEL_TOOLS")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .or(budgets.max_parallel_tools)
            .unwrap_or(DEFAULT_MAX_PARALLEL_TOOLS);
        let tool_spill_threshold_bytes = parse_usize(
            get_var("SQUEEZY_TOOL_SPILL_THRESHOLD_BYTES"),
            budgets
                .tool_spill_threshold_bytes
                .unwrap_or(DEFAULT_TOOL_SPILL_THRESHOLD_BYTES),
        );
        let tool_preview_bytes = parse_usize(
            get_var("SQUEEZY_TOOL_PREVIEW_BYTES"),
            budgets
                .tool_preview_bytes
                .unwrap_or(DEFAULT_TOOL_PREVIEW_BYTES),
        );
        let max_tool_result_bytes_per_round = parse_usize(
            get_var("SQUEEZY_MAX_TOOL_RESULT_BYTES_PER_ROUND"),
            budgets
                .max_tool_result_bytes_per_round
                .unwrap_or(DEFAULT_MAX_TOOL_RESULT_BYTES_PER_ROUND),
        );
        let tool_output_retention_days = get_var("SQUEEZY_TOOL_OUTPUT_RETENTION_DAYS")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .or(budgets.tool_output_retention_days)
            .unwrap_or(DEFAULT_TOOL_OUTPUT_RETENTION_DAYS);
        let max_tool_calls_per_turn = parse_u64(
            get_var("SQUEEZY_MAX_TOOL_CALLS_PER_TURN"),
            budgets
                .max_tool_calls_per_turn
                .unwrap_or(DEFAULT_MAX_TOOL_CALLS_PER_TURN),
        );
        let max_tool_bytes_read_per_turn = parse_u64(
            get_var("SQUEEZY_MAX_TOOL_BYTES_READ_PER_TURN"),
            budgets
                .max_tool_bytes_read_per_turn
                .unwrap_or(DEFAULT_MAX_TOOL_BYTES_READ_PER_TURN),
        );
        let max_search_files_per_turn = parse_u64(
            get_var("SQUEEZY_MAX_SEARCH_FILES_PER_TURN"),
            budgets
                .max_search_files_per_turn
                .unwrap_or(DEFAULT_MAX_SEARCH_FILES_PER_TURN),
        );
        let max_session_cost_usd_micros = get_var("SQUEEZY_MAX_SESSION_COST_USD_MICROS")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .or(budgets.max_session_cost_usd_micros);
        let cost_warn_percent = get_var("SQUEEZY_COST_WARN_PERCENT")
            .and_then(|value| value.parse::<u8>().ok())
            .filter(|value| (1..=100).contains(value))
            .or(budgets.cost_warn_percent)
            .unwrap_or(DEFAULT_COST_WARN_PERCENT);
        let disable_prompt_cache = get_var("SQUEEZY_DISABLE_PROMPT_CACHE")
            .as_deref()
            .map(parse_enabled_bool)
            .unwrap_or(false);
        let max_round_input_tokens = get_var("SQUEEZY_MAX_ROUND_INPUT_TOKENS")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .or(budgets.max_round_input_tokens);
        // Global routing config. Per-provider model overrides
        // (`[providers.<slug>].cheap_model` / `judge_model` / `judge_prompt` /
        // `expensive_models`) are layered on top at the use-site resolvers in
        // squeezy-agent (`cheap_model_for` / `judge_model_for` / …) which read
        // `AppConfig.providers` — that keeps live config-screen edits and
        // provider switches resolving without rebuilding the whole config.
        let routing = RoutingConfig::from_settings_and_env(
            settings.routing.unwrap_or_default(),
            &mut get_var,
        );
        let telemetry = TelemetryConfig::from_settings_and_env(
            settings.telemetry.unwrap_or_default(),
            &mut get_var,
        );
        let feedback = FeedbackConfig::from_settings_and_env(
            settings.feedback.unwrap_or_default(),
            &mut get_var,
        );
        let redaction = RedactionConfig::from_settings(settings.redaction.unwrap_or_default())?;
        let mcp_servers = settings.mcp.map(|mcp| mcp.servers).unwrap_or_default();
        let mut permission_settings = settings.permissions.unwrap_or_default();
        // Insert MCP-derived rules *before* the user's explicit
        // `[[permissions.rules]]`. Permission matching is "last rule wins",
        // so this keeps any deliberate user deny/allow as the final word
        // and prevents an MCP server's own permission block from silently
        // overriding admin policy.
        let mut combined_rules = mcp_permission_rules(&mcp_servers);
        combined_rules.append(&mut permission_settings.rules);
        permission_settings.rules = combined_rules;
        let permissions = PermissionPolicy::from_settings_and_env(
            permission_settings,
            &sources.join(","),
            &workspace_root,
            &mut get_var,
        )?;
        let mut session_settings = settings.session.unwrap_or_default();
        let session_mode = parse_session_mode(
            get_var("SQUEEZY_SESSION_MODE"),
            session_settings.mode.unwrap_or_default(),
        );
        // `SQUEEZY_SESSION_DIR` overrides `[session].log_dir` so operators can
        // redirect session traces (CI runners, ephemeral sandboxes, multi-user
        // hosts) without rewriting settings.toml. Whitespace-only values are
        // treated as unset so a stray `export SQUEEZY_SESSION_DIR=` cannot
        // silently clear a configured directory. The CLI `--session-dir`
        // overlay in `squeezy-cli` mutates `AppConfig.session_logs.log_dir`
        // directly on top of this resolved value, giving the final order
        // flag > env > config > default.
        if let Some(raw) = get_var("SQUEEZY_SESSION_DIR") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                session_settings.log_dir = Some(PathBuf::from(trimmed));
            }
        }
        let mut skills = SkillsConfig::from_settings_and_env_vars(
            settings.skills.unwrap_or_default(),
            &mut get_var,
        );
        let graph = GraphConfig::from_settings(settings.graph.unwrap_or_default());
        let cache = CacheConfig::from_settings(settings.cache.unwrap_or_default());
        let tool_settings = settings.tools.unwrap_or_default();
        let checkpoints_enabled = get_var("SQUEEZY_CHECKPOINTS_ENABLED")
            .as_deref()
            .map(parse_enabled_bool)
            .unwrap_or(tool_settings.checkpoints_enabled.unwrap_or(false));
        let tools = ToolSchemaConfig::from_settings(tool_settings)?;
        let session_resume_picker = session_settings.resume_picker.unwrap_or_default();
        let session_logs = SessionLogConfig::from_settings(&session_settings);
        let context_compaction = ContextCompactionConfig::from_settings_and_env(
            settings.context.unwrap_or_default(),
            &mut get_var,
        );
        // Skills' ContextPercent budget mode reads the same window value as
        // mid-turn compaction; resolve it here instead of duplicating the
        // env/file precedence logic.
        skills.model_context_window = context_compaction.model_context_window;
        let subagents = SubagentConfig::from_settings_and_env(
            settings.subagents.unwrap_or_default(),
            &mut get_var,
        );
        let tui = TuiConfig::from_settings(settings.tui.unwrap_or_default());
        if env_used {
            sources.push("env".to_string());
        }
        if cli_provider.is_some() && !sources.iter().any(|source| source == "cli") {
            sources.push("cli".to_string());
        }
        Ok(Self {
            provider,
            model,
            small_fast_model,
            profile,
            reasoning_effort,
            instructions: DEFAULT_INSTRUCTIONS.to_string(),
            max_output_tokens,
            temperature,
            top_p,
            seed,
            stop,
            frequency_penalty,
            presence_penalty,
            tool_choice,
            parallel_tool_calls,
            batch_tool_calls_hint,
            stream_idle_timeout: Duration::from_millis(stream_idle_timeout_ms),
            tick_rate: Duration::from_millis(tui.tick_rate_ms),
            workspace_root,
            permissions,
            session_mode,
            session_resume_picker,
            session_logs,
            context_compaction,
            subagents,
            store_responses,
            exploration_graph,
            max_parallel_tools,
            tool_spill_threshold_bytes,
            tool_preview_bytes,
            max_tool_result_bytes_per_round,
            tool_output_retention_days,
            exa_mcp_url,
            exa_api_key_env,
            parallel_mcp_url,
            parallel_api_key_env,
            websearch_provider,
            max_tool_calls_per_turn,
            max_tool_bytes_read_per_turn,
            max_search_files_per_turn,
            max_session_cost_usd_micros,
            cost_warn_percent,
            disable_prompt_cache,
            max_round_input_tokens,
            routing,
            telemetry,
            feedback,
            redaction,
            skills,
            graph,
            cache,
            tools,
            checkpoints_enabled,
            tui,
            mcp_servers,
            providers,
            model_limits,
            hardening: HardeningConfig::from_settings(settings.hardening.unwrap_or_default()),
            config_sources: sources,
            config_warnings,
        })
    }

    fn from_default_paths_and_env_with_provider_value(
        provider: Option<&str>,
        profile: Option<&str>,
    ) -> Result<Self> {
        let (mut settings, mut sources, warnings) = load_default_settings_sources()?;
        if let Some(name) = profile {
            settings.apply_profile(name)?;
            sources.push(format!("profile:{name}"));
        }
        Self::try_from_settings_and_env_vars_with_sources_and_warnings(
            settings,
            sources,
            warnings,
            provider,
            |name| env::var(name).ok(),
        )
    }

    fn built_in_defaults() -> Self {
        Self::try_from_settings_and_env_vars(SettingsFile::default(), None, |_| None)
            .expect("built-in config defaults are valid")
    }

    /// Recompute warnings that depend on effective provider/model values.
    ///
    /// The config screen mutates an `AppConfig` in memory before arming the
    /// next-prompt swap. Recomputing only generated provider/model warnings
    /// keeps those edits honest without discarding parse or unknown-field
    /// warnings that came from TOML loading.
    pub fn refresh_config_warnings(&mut self) {
        self.config_warnings
            .retain(|warning| !is_generated_provider_model_warning(warning));
        add_provider_model_warnings(
            &self.provider,
            &self.model,
            ModelSamplingOptions {
                temperature: self.temperature,
                top_p: self.top_p,
                seed: self.seed,
                stop_set: !self.stop.is_empty(),
                frequency_penalty: self.frequency_penalty,
                presence_penalty: self.presence_penalty,
            },
            self.max_output_tokens,
            &mut self.config_warnings,
        );
    }

    /// Resolves the small-fast-model id for background calls (compaction
    /// summary, classifier, auto-approver). Returns the user-configured value
    /// when set, then the provider built-in cheap default, then `None` when
    /// the provider has no curated cheap tier — callers fall back to the
    /// main model in that case.
    pub fn resolved_small_fast_model(&self) -> Option<String> {
        if let Some(model) = &self.small_fast_model {
            return Some(model.clone());
        }
        small_fast_model_for_provider(provider_kind(&self.provider)).map(str::to_string)
    }

    /// Returns `config_sources` with file paths reduced to short labels
    /// (`"user"`, `"project"`, `"repo"`) for display in narrow status lines. Full
    /// paths remain available on `config_sources` and via `config inspect`.
    pub fn config_source_labels(&self) -> Vec<&str> {
        self.config_sources
            .iter()
            .map(|source| match source.split_once(':') {
                Some((label, _)) => label,
                None => source.as_str(),
            })
            .collect()
    }

    /// Returns a TOML-shaped report of the effective configuration with
    /// sensitive values redacted. The output is valid TOML and the same
    /// document can be parsed back by `SettingsFile::from_toml_str`
    /// (note: `[graph]` and `[mcp.servers.*]` sections currently round-trip
    /// into the typed model but no consumer reads them yet).
    pub fn inspect_redacted(&self) -> String {
        let mut output = String::new();
        output.push_str("# effective Squeezy config\n");
        // sources is a debug artifact, emitted as a comment so the document
        // round-trips through SettingsFile::from_toml_str without choking on
        // a key that does not belong in user-authored settings.
        output.push_str(&format!(
            "# sources = {}\n\n",
            toml_string_array(&self.config_sources)
        ));

        output.push_str("[model]\n");
        output.push_str(&format!(
            "provider = {}\n",
            toml_string(provider_kind(&self.provider))
        ));
        output.push_str(&format!("model = {}\n", toml_string(&self.model)));
        if let Some(small_fast) = &self.small_fast_model {
            output.push_str(&format!("small_fast_model = {}\n", toml_string(small_fast)));
        } else {
            output.push_str(
                "# small_fast_model = unset  # uses per-provider built-in cheap default\n",
            );
        }
        output.push_str(&format!(
            "profile = {}\n",
            toml_string(self.profile.as_str())
        ));
        if let Some(reasoning_effort) = self.reasoning_effort {
            output.push_str(&format!(
                "reasoning_effort = {}\n",
                toml_string(reasoning_effort.as_str())
            ));
        }
        if let Some(max_output_tokens) = self.max_output_tokens {
            output.push_str(&format!("max_output_tokens = {max_output_tokens}\n"));
        } else {
            output.push_str(
                "# max_output_tokens = unset  # no Squeezy cap; provider/model limit applies\n",
            );
        }
        if let Some(temperature) = self.temperature {
            output.push_str(&format!("temperature = {}\n", format_f32(temperature)));
        } else {
            output.push_str("# temperature = unset  # provider/model default\n");
        }
        if let Some(top_p) = self.top_p {
            output.push_str(&format!("top_p = {}\n", format_f32(top_p)));
        } else {
            output.push_str("# top_p = unset  # provider/model default\n");
        }
        if let Some(seed) = self.seed {
            output.push_str(&format!("seed = {seed}\n"));
        } else {
            output.push_str("# seed = unset  # provider/model default\n");
        }
        if self.stop.is_empty() {
            output.push_str(
                "# stop = []  # provider/model default; add strings to stop generation early\n",
            );
        } else {
            output.push_str(&format!("stop = {}\n", toml_string_array(&self.stop)));
        }
        if let Some(frequency_penalty) = self.frequency_penalty {
            output.push_str(&format!(
                "frequency_penalty = {}\n",
                format_f32(frequency_penalty)
            ));
        } else {
            output.push_str("# frequency_penalty = unset  # provider/model default\n");
        }
        if let Some(presence_penalty) = self.presence_penalty {
            output.push_str(&format!(
                "presence_penalty = {}\n",
                format_f32(presence_penalty)
            ));
        } else {
            output.push_str("# presence_penalty = unset  # provider/model default\n");
        }
        match self.parallel_tool_calls {
            Some(value) => {
                output.push_str(&format!("parallel_tool_calls = {value}\n"));
            }
            None => {
                output.push_str(
                    "# parallel_tool_calls = unset  # provider default (parallel on OpenAI)\n",
                );
            }
        }
        output.push_str(&format!(
            "batch_tool_calls_hint = {}\n",
            self.batch_tool_calls_hint
        ));
        output.push_str(&format!(
            "stream_idle_timeout_ms = {}\n",
            self.stream_idle_timeout.as_millis()
        ));
        output.push_str(&format!("store_responses = {}\n\n", self.store_responses));

        output.push_str("[agent]\n");
        output.push_str(&format!(
            "exploration_graph = {}\n\n",
            self.exploration_graph
        ));

        output.push_str("[session]\n");
        output.push_str(&format!(
            "mode = {}\n",
            toml_string(self.session_mode.as_str())
        ));
        output.push_str(&format!(
            "resume_picker = {}\n",
            toml_string(self.session_resume_picker.as_str())
        ));
        if let Some(log_dir) = &self.session_logs.log_dir {
            output.push_str(&format!(
                "log_dir = {}\n",
                toml_string(&log_dir.display().to_string())
            ));
        }
        output.push_str(&format!(
            "log_retention_days = {}\n",
            self.session_logs.log_retention_days
        ));
        output.push_str(&format!(
            "log_retention_archive_days = {}\n",
            self.session_logs.log_retention_archive_days
        ));
        output.push_str(&format!(
            "max_event_bytes = {}\n",
            self.session_logs.max_event_bytes
        ));
        output.push_str(&format!(
            "max_session_bytes = {}\n\n",
            self.session_logs.max_session_bytes
        ));

        output.push_str("[context]\n");
        output.push_str(&format!(
            "compaction_enabled = {}\n",
            self.context_compaction.enabled
        ));
        output.push_str(&format!(
            "fallback_window_tokens = {}\n",
            self.context_compaction.fallback_window_tokens
        ));
        if let Some(cap) = self.context_compaction.max_context_tokens {
            output.push_str(&format!("max_context_tokens = {}\n", cap));
        }
        output.push_str(&format!(
            "compaction_min_items = {}\n",
            self.context_compaction.min_items
        ));
        output.push_str(&format!(
            "compaction_recent_items = {}\n",
            self.context_compaction.recent_items
        ));
        output.push_str(&format!(
            "compaction_max_summary_bytes = {}\n",
            self.context_compaction.max_summary_bytes
        ));
        output.push_str(&format!(
            "repo_doc_max_bytes = {}\n",
            self.context_compaction.repo_doc_max_bytes
        ));
        output.push_str(&format!(
            "user_memory_max_bytes = {}\n",
            self.context_compaction.user_memory_max_bytes
        ));
        output.push_str(&format!(
            "enabled_mid_turn = {}\n",
            self.context_compaction.enabled_mid_turn
        ));
        if let Some(window) = self.context_compaction.model_context_window {
            output.push_str(&format!("model_context_window = {}\n", window));
        }
        if let Some(percent) = self.context_compaction.effective_context_window_percent {
            output.push_str(&format!("effective_context_window_percent = {}\n", percent));
        }
        if let Some(reserve) = self.context_compaction.baseline_reserve_tokens {
            output.push_str(&format!("baseline_reserve_tokens = {}\n", reserve));
        }
        output.push_str(&format!(
            "warn_at_percent = {}\n",
            self.context_compaction.warn_at_percent
        ));
        output.push_str(&format!(
            "strategy = {}\n",
            toml_string(self.context_compaction.strategy.as_str())
        ));
        if let Some(model) = &self.context_compaction.model_assisted_model {
            output.push_str(&format!("model_assisted_model = {}\n", toml_string(model)));
        }
        output.push_str(&format!(
            "model_assisted_max_output_tokens = {}\n",
            self.context_compaction.model_assisted_max_output_tokens
        ));
        output.push_str(&format!(
            "model_assisted_timeout_secs = {}\n",
            self.context_compaction.model_assisted_timeout_secs
        ));
        output.push_str(&format!(
            "layered_fallback_extractive_threshold_tokens = {}\n",
            self.context_compaction
                .layered_fallback_extractive_threshold_tokens
        ));
        output.push_str(&format!(
            "micro_compaction_enabled = {}\n",
            self.context_compaction.micro_compaction_enabled
        ));
        output.push_str(&format!(
            "trim_at_percent = {}\n",
            self.context_compaction.trim_at_percent
        ));
        output.push_str(&format!(
            "micro_compaction_keep_recent = {}\n\n",
            self.context_compaction.micro_compaction_keep_recent
        ));

        output.push_str("[subagents]\n");
        output.push_str(&format!("enabled = {}\n", self.subagents.enabled));
        output.push_str(&format!(
            "explore_enabled = {}\n",
            self.subagents.explore_enabled
        ));
        if let Some(model) = &self.subagents.explore_model {
            output.push_str(&format!("explore_model = {}\n", toml_string(model)));
        }
        output.push_str(&format!(
            "max_concurrent = {}\n",
            self.subagents.max_concurrent
        ));
        output.push_str(&format!(
            "max_tool_calls_per_call = {}\n",
            self.subagents.max_tool_calls_per_call
        ));
        output.push_str(&format!(
            "max_tool_bytes_read_per_call = {}\n",
            self.subagents.max_tool_bytes_read_per_call
        ));
        output.push_str(&format!(
            "max_search_files_per_call = {}\n",
            self.subagents.max_search_files_per_call
        ));
        output.push_str(&format!(
            "max_model_rounds = {}\n",
            self.subagents.max_model_rounds
        ));
        output.push_str(&format!(
            "max_summary_tokens = {}\n",
            self.subagents.max_summary_tokens
        ));
        match self.subagents.max_runtime_secs {
            Some(secs) => output.push_str(&format!("max_runtime_secs = {secs}\n")),
            None => output.push_str("max_runtime_secs = 0  # disabled; no wall-clock cap\n"),
        }
        output.push_str(&format!(
            "include_transcript = {}\n\n",
            self.subagents.include_transcript
        ));

        output.push_str("[budgets]\n");
        output.push_str(&format!(
            "max_parallel_tools = {}\n",
            self.max_parallel_tools
        ));
        output.push_str(&format!(
            "max_tool_calls_per_turn = {}\n",
            self.max_tool_calls_per_turn
        ));
        output.push_str(&format!(
            "max_tool_bytes_read_per_turn = {}\n",
            self.max_tool_bytes_read_per_turn
        ));
        output.push_str(&format!(
            "max_search_files_per_turn = {}\n",
            self.max_search_files_per_turn
        ));
        output.push_str(&format!(
            "max_tool_result_bytes_per_round = {}\n\n",
            self.max_tool_result_bytes_per_round
        ));
        if let Some(max_session_cost_usd_micros) = self.max_session_cost_usd_micros {
            output.push_str(&format!(
                "max_session_cost_usd_micros = {max_session_cost_usd_micros}\n"
            ));
        } else {
            output.push_str("# max_session_cost_usd_micros = unset\n");
        }
        output.push_str(&format!("cost_warn_percent = {}\n", self.cost_warn_percent));
        if let Some(max_round_input_tokens) = self.max_round_input_tokens {
            output.push_str(&format!(
                "max_round_input_tokens = {max_round_input_tokens}\n\n"
            ));
        } else {
            output.push_str("# max_round_input_tokens = unset\n\n");
        }

        output.push_str("[routing]\n");
        output.push_str(
            "# Global routing toggles. Per-provider model overrides (cheap_model,\n\
             # judge_model, judge_prompt, expensive_models) live under [providers.<name>]\n\
             # because routing never crosses providers.\n",
        );
        output.push_str(&format!("enabled = {}\n", self.routing.enabled));
        output.push_str(&format!("heuristic = {}\n", self.routing.heuristic));
        output.push_str(&format!("llm_judge = {}\n", self.routing.llm_judge));
        output.push_str(&format!(
            "follow_up_max_chars = {}  # short follow-ups inherit the prior turn's route\n",
            self.routing.follow_up_max_chars
        ));
        if self.routing.cheap_escalation_tool_calls == 0 {
            output.push_str(
                "# cheap_escalation_tool_calls = 0  # derives at runtime as max_tool_calls_per_turn / 4\n",
            );
        } else {
            output.push_str(&format!(
                "cheap_escalation_tool_calls = {}\n",
                self.routing.cheap_escalation_tool_calls
            ));
        }
        output.push_str(&format!(
            "cheap_escalation_error_threshold = {}\n",
            self.routing.cheap_escalation_error_threshold
        ));
        output.push_str(&format!(
            "escalation_sticky_turns = {}\n",
            self.routing.escalation_sticky_turns
        ));
        output.push_str(&format!(
            "bypass_for_images = {}\n",
            self.routing.bypass_for_images
        ));
        output.push_str(&format!(
            "large_attachment_bypass_bytes = {}\n",
            self.routing.large_attachment_bypass_bytes
        ));
        output.push_str(&format!(
            "heuristic_max_chars = {}\n",
            self.routing.heuristic_max_chars
        ));
        output.push_str(&format!(
            "judge_max_chars = {}\n",
            self.routing.judge_max_chars
        ));
        // judge_model / judge_prompt / expensive_models below show the value
        // resolved for the ACTIVE provider; set them per provider under
        // [providers.<name>] (a global [routing] value applies as a fallback).
        if let Some(judge_model) = &self.routing.judge_model {
            output.push_str(&format!("judge_model = {}\n", toml_string(judge_model)));
        } else {
            output.push_str(
                "# judge_model = unset  # defaults to the per-provider mini tier (must be cheap)\n",
            );
        }
        if let Some(judge_prompt) = &self.routing.judge_prompt {
            output.push_str(&format!("judge_prompt = {}\n", toml_string(judge_prompt)));
        } else {
            output.push_str(
                "# judge_prompt = unset  # defaults to the built-in per-provider prompt\n",
            );
        }
        if self.routing.expensive_models.is_empty() {
            output.push_str(
                "# expensive_models = \"(?i)^(?!.*(nano|mini)).*\"  # regex; reroute when the parent matches. Per-provider default skips cheap tiers (haiku/mini/nano/flash)\n",
            );
        } else {
            output.push_str(&format!(
                "expensive_models = {}\n",
                toml_string(&self.routing.expensive_models)
            ));
        }
        if self.routing.extra_heuristic_verbs.is_empty() {
            output.push_str("# extra_heuristic_verbs = []  # user-extended verb whitelist\n\n");
        } else {
            output.push_str(&format!(
                "extra_heuristic_verbs = {}\n\n",
                toml_string_array(&self.routing.extra_heuristic_verbs)
            ));
        }

        output.push_str("[permissions]\n");
        output.push_str(&format!(
            "mode = {}\n",
            toml_string(self.permissions.mode.as_str())
        ));
        output.push_str(&format!(
            "read = {}\n",
            toml_string(self.permissions.read.as_str())
        ));
        output.push_str(&format!(
            "search = {}\n",
            toml_string(self.permissions.search.as_str())
        ));
        output.push_str(&format!(
            "edit = {}\n",
            toml_string(self.permissions.edit.as_str())
        ));
        output.push_str(&format!(
            "shell = {}\n",
            toml_string(self.permissions.shell.as_str())
        ));
        output.push_str(&format!(
            "ignored_search = {}\n",
            toml_string(self.permissions.ignored_search.as_str())
        ));
        output.push_str(&format!(
            "web = {}\n",
            toml_string(self.permissions.web.as_str())
        ));
        output.push_str(&format!(
            "mcp = {}\n",
            toml_string(self.permissions.mcp.as_str())
        ));
        output.push_str(&format!(
            "git = {}\n",
            toml_string(self.permissions.git.as_str())
        ));
        output.push_str(&format!(
            "compiler = {}\n",
            toml_string(self.permissions.compiler.as_str())
        ));
        output.push_str(&format!(
            "destructive = {}\n",
            toml_string(self.permissions.destructive.as_str())
        ));
        output.push_str(&format!(
            "shell_classifier = {}\n\n",
            self.permissions.shell_classifier
        ));
        output.push_str("[permissions.ai_reviewer]\n");
        output.push_str(&format!(
            "enabled = {}\n",
            self.permissions.ai_reviewer.enabled
        ));
        if let Some(model) = &self.permissions.ai_reviewer.model {
            output.push_str(&format!("model = {}\n", toml_string(model)));
        }
        output.push_str(&format!(
            "allow_capabilities = {}\n",
            toml_string_array(
                &self
                    .permissions
                    .ai_reviewer
                    .allow_capabilities
                    .iter()
                    .map(|capability| capability.as_str().to_string())
                    .collect::<Vec<_>>()
            )
        ));
        if let Some(policy_file) = &self.permissions.ai_reviewer.policy_file {
            output.push_str(&format!(
                "policy_file = {}\n",
                toml_string(&policy_file.display().to_string())
            ));
        }
        if let Some(policy) = &self.permissions.ai_reviewer.policy {
            output.push_str(&format!("policy = {}\n", toml_string(policy)));
        }
        output.push_str(&format!(
            "max_transcript_tokens = {}\n",
            self.permissions.ai_reviewer.max_transcript_tokens
        ));
        output.push_str(&format!(
            "timeout_secs = {}\n\n",
            self.permissions.ai_reviewer.timeout_secs
        ));
        output.push_str("[permissions.shell_sandbox]\n");
        output.push_str(&format!(
            "mode = {}\n",
            toml_string(self.permissions.shell_sandbox.mode.as_str())
        ));
        output.push_str(&format!(
            "network = {}\n",
            toml_string(self.permissions.shell_sandbox.network.as_str())
        ));
        output.push_str(&format!(
            "audit = {}\n",
            self.permissions.shell_sandbox.audit
        ));
        output.push_str(&format!(
            "kill_grace_ms = {}\n",
            self.permissions.shell_sandbox.kill_grace_ms
        ));
        output.push_str(&format!(
            "env_allowlist = {}\n",
            toml_string_array(&self.permissions.shell_sandbox.env_allowlist)
        ));
        output.push_str(&format!(
            "read_roots = {}\n",
            toml_path_array(&self.permissions.shell_sandbox.read_roots)
        ));
        output.push_str(&format!(
            "write_roots = {}\n",
            toml_path_array(&self.permissions.shell_sandbox.write_roots)
        ));
        output.push_str(&format!(
            "protected_metadata_names = {}\n",
            toml_string_array(&self.permissions.shell_sandbox.protected_metadata_names)
        ));
        output.push_str(&format!(
            "sensitive_path_patterns = {}\n",
            toml_string_array(&self.permissions.shell_sandbox.sensitive_path_patterns)
        ));
        // The list above is the EFFECTIVE list (built-in floor unioned with
        // user additions). Round-tripping must not re-union, otherwise an
        // inspected config would diverge from the running config.
        output.push_str("replace_sensitive_path_patterns = true\n\n");
        for rule in self
            .permissions
            .rules
            .iter()
            .filter(|rule| rule.source != PermissionRuleSource::Builtin)
        {
            output.push_str("[[permissions.rules]]\n");
            output.push_str(&format!("capability = {}\n", toml_string(&rule.capability)));
            output.push_str(&format!("target = {}\n", toml_string(&rule.target)));
            output.push_str(&format!("action = {}\n", toml_string(rule.action.as_str())));
            output.push_str(&format!("source = {}\n", toml_string(rule.source.as_str())));
            if let Some(reason) = &rule.reason {
                output.push_str(&format!("reason = {}\n", toml_string(reason)));
            }
            output.push('\n');
        }

        output.push_str("[hardening]\n");
        output.push_str(&format!(
            "disable_core_dumps = {}\n",
            self.hardening.disable_core_dumps
        ));
        output.push_str(&format!(
            "deny_debug_attach = {}\n\n",
            self.hardening.deny_debug_attach
        ));

        output.push_str("[telemetry]\n");
        output.push_str(&format!("enabled = {}\n", self.telemetry.enabled));
        output.push_str(&format!(
            "endpoint = {}\n\n",
            toml_string(&self.telemetry.endpoint)
        ));

        output.push_str("[feedback]\n");
        output.push_str(&format!("enabled = {}\n", self.feedback.enabled));
        output.push_str(&format!(
            "feedback_endpoint = {}\n",
            toml_string(&self.feedback.feedback_endpoint)
        ));
        output.push_str(&format!(
            "report_endpoint = {}\n",
            toml_string(&self.feedback.report_endpoint)
        ));
        output.push_str(&format!(
            "max_feedback_bytes = {}\n",
            self.feedback.max_feedback_bytes
        ));
        output.push_str(&format!(
            "max_report_bytes = {}\n\n",
            self.feedback.max_report_bytes
        ));

        output.push_str("[redaction]\n");
        if self.redaction.custom_patterns.is_empty() {
            output.push_str("custom_patterns = []\n\n");
        } else {
            // Emit a TOML comment so the document still round-trips through
            // `SettingsFile::from_toml_str`, but do not echo the literal
            // patterns. A previous version emitted
            // `custom_patterns = ["<redacted>"]`, which was itself a valid
            // (no-op) regex if pasted back into a settings file.
            output.push_str(&format!(
                "# {} custom redaction pattern(s) hidden in inspect output\n",
                self.redaction.custom_patterns.len(),
            ));
            output.push_str("custom_patterns = []\n\n");
        }

        output.push_str("[web]\n");
        output.push_str(&format!(
            "websearch_provider = {}\n",
            toml_string(&self.websearch_provider)
        ));
        output.push_str(&format!(
            "exa_mcp_url = {}\n",
            toml_string(&self.exa_mcp_url)
        ));
        output.push_str("exa_api_key_env = \"<redacted>\"\n");
        output.push_str(&format!(
            "parallel_mcp_url = {}\n",
            toml_string(&self.parallel_mcp_url)
        ));
        output.push_str("parallel_api_key_env = \"<redacted>\"\n\n");

        output.push_str("[skills]\n");
        output.push_str(&format!(
            "user_dir = {}\n",
            toml_string(&self.skills.user_dir.display().to_string())
        ));
        output.push_str(&format!(
            "compat_user_dir = {}\n",
            toml_string(&self.skills.compat_user_dir.display().to_string())
        ));
        output.push_str(&format!(
            "active_budget_chars = {}\n",
            self.skills.active_budget_chars
        ));
        output.push_str(&format!(
            "active_body_cap_chars = {}\n",
            self.skills.active_body_cap_chars
        ));
        output.push_str(&format!(
            "preamble_enabled = {}\n",
            self.skills.preamble_enabled
        ));
        output.push_str(&format!(
            "preamble_budget_chars = {}\n",
            self.skills.preamble_budget_chars
        ));
        output.push_str(&format!("inline = {}\n", self.skills.inline));
        output.push_str(&format!("hooks_enabled = {}\n", self.skills.hooks_enabled));
        // The mode tables follow the same inline-table shape that
        // `from_table` accepts, so the inspect output round-trips when
        // pasted back into a settings file.
        emit_skills_budget_mode(
            &mut output,
            "active_budget_mode",
            self.skills.active_budget_mode,
        );
        emit_skills_budget_mode(
            &mut output,
            "preamble_budget_mode",
            self.skills.preamble_budget_mode,
        );
        if self.skills.config.is_empty() {
            output.push('\n');
        } else {
            output.push('\n');
            for entry in &self.skills.config {
                output.push_str("[[skills.config]]\n");
                if let Some(name) = &entry.name {
                    output.push_str(&format!("name = {}\n", toml_string(name)));
                }
                if let Some(path) = &entry.path {
                    output.push_str(&format!(
                        "path = {}\n",
                        toml_string(&path.display().to_string())
                    ));
                }
                output.push_str(&format!("enabled = {}\n\n", entry.enabled));
            }
        }

        output.push_str("[graph]\n");
        output.push_str(&format!(
            "languages = {}\n",
            toml_string_array(&self.graph.languages)
        ));
        output.push_str(&format!("max_file_bytes = {}\n", self.graph.max_file_bytes));
        output.push_str(&format!("include_hidden = {}\n", self.graph.include_hidden));
        output.push_str(&format!(
            "require_indexing_signal = {}\n\n",
            self.graph.require_indexing_signal
        ));
        output.push_str(&format!(
            "include = {}\n",
            toml_string_array(&self.graph.include)
        ));
        output.push_str(&format!(
            "exclude = {}\n",
            toml_string_array(&self.graph.exclude)
        ));
        output.push_str(&format!(
            "include_classes = {}\n",
            toml_string_array(&self.graph.include_classes)
        ));
        output.push_str(&format!(
            "exclude_classes = {}\n\n",
            toml_string_array(&self.graph.exclude_classes)
        ));

        output.push_str("[cache]\n");
        if let Some(root) = &self.cache.root {
            output.push_str(&format!(
                "root = {}\n",
                toml_string(&root.display().to_string())
            ));
        }
        if let Some(tool_outputs) = &self.cache.tool_outputs {
            output.push_str(&format!(
                "tool_outputs = {}\n",
                toml_string(&tool_outputs.display().to_string())
            ));
        }
        output.push('\n');

        output.push_str("[tools]\n");
        output.push_str(&format!(
            "checkpoints_enabled = {}\n",
            self.checkpoints_enabled
        ));
        output.push_str(&format!(
            "lazy_schema_loading = {}\n",
            self.tools.lazy_schema_loading
        ));
        output.push_str(&format!("core = {}\n", toml_string_array(&self.tools.core)));
        output.push_str(&format!(
            "discoverable = {}\n",
            toml_string_array(&self.tools.discoverable)
        ));
        output.push_str(&format!(
            "excluded = {}\n\n",
            toml_string_array(&self.tools.excluded)
        ));

        output.push_str("[tui]\n");
        output.push_str(&format!("tick_rate_ms = {}\n", self.tui.tick_rate_ms));
        output.push_str(&format!(
            "status_verbosity = {}\n",
            toml_string(self.tui.status_verbosity.as_str())
        ));
        output.push_str(&format!(
            "response_verbosity = {}\n",
            toml_string(self.tui.response_verbosity.as_str())
        ));
        output.push_str(&format!(
            "tool_output_verbosity = {}\n",
            toml_string(self.tui.tool_output_verbosity.as_str())
        ));
        output.push_str(&format!(
            "transcript_default = {}\n",
            toml_string(self.tui.transcript_default.as_str())
        ));
        output.push_str(&format!(
            "synchronized_output = {}\n",
            toml_string(self.tui.synchronized_output.as_str())
        ));
        output.push_str(&format!("theme = {}\n", toml_string(&self.tui.theme)));
        output.push_str(&format!(
            "show_reasoning_usage = {}\n",
            self.tui.show_reasoning_usage
        ));
        output.push_str(&format!(
            "persist_prompt_history = {}\n\n",
            self.tui.persist_prompt_history
        ));
        for (name, theme) in &self.tui.themes {
            if theme.colors.is_empty() {
                continue;
            }
            output.push_str(&format!(
                "[tui.themes.{}.colors]\n",
                toml_bare_or_quoted_key(name)
            ));
            for (token, rgb) in &theme.colors {
                output.push_str(&format!(
                    "{} = [{}, {}, {}]\n",
                    toml_bare_or_quoted_key(token),
                    rgb[0],
                    rgb[1],
                    rgb[2]
                ));
            }
            output.push('\n');
        }

        for (name, server) in &self.mcp_servers {
            output.push_str(&format!(
                "[mcp.servers.{}]\n",
                toml_bare_or_quoted_key(name)
            ));
            output.push_str(&format!("enabled = {}\n", server.enabled));
            output.push_str(&format!(
                "transport = {}\n",
                toml_string(server.transport.as_str())
            ));
            if let Some(command) = &server.command {
                output.push_str(&format!("command = {}\n", toml_string(command)));
            }
            output.push_str(&format!("args = {}\n", toml_string_array(&server.args)));
            if let Some(url) = &server.url {
                output.push_str(&format!("url = {}\n", toml_string(url)));
            }
            if let Some(timeout_ms) = server.timeout_ms {
                output.push_str(&format!("timeout_ms = {timeout_ms}\n"));
            }
            if let Some(discovery_timeout_ms) = server.discovery_timeout_ms {
                output.push_str(&format!("discovery_timeout_ms = {discovery_timeout_ms}\n"));
            }
            if let Some(tool_call_timeout_ms) = server.tool_call_timeout_ms {
                output.push_str(&format!("tool_call_timeout_ms = {tool_call_timeout_ms}\n"));
            }
            if let Some(enabled_tools) = &server.enabled_tools {
                output.push_str(&format!(
                    "enabled_tools = {}\n",
                    toml_string_array(enabled_tools)
                ));
            }
            if !server.disabled_tools.is_empty() {
                output.push_str(&format!(
                    "disabled_tools = {}\n",
                    toml_string_array(&server.disabled_tools)
                ));
            }
            if !server.env.is_empty() {
                let entries = server
                    .env
                    .keys()
                    .map(|key| {
                        format!(
                            "{} = {}",
                            toml_bare_or_quoted_key(key),
                            toml_string("<redacted>")
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                output.push_str(&format!("env = {{ {entries} }}\n"));
            }
            if let Some(default) = server.permissions.default {
                output.push('\n');
                output.push_str(&format!(
                    "[mcp.servers.{}.permissions]\n",
                    toml_bare_or_quoted_key(name)
                ));
                output.push_str(&format!("default = {}\n", toml_string(default.as_str())));
            }
            for rule in &server.permissions.rules {
                output.push('\n');
                output.push_str(&format!(
                    "[[mcp.servers.{}.permissions.rules]]\n",
                    toml_bare_or_quoted_key(name)
                ));
                let target = rule
                    .target
                    .strip_prefix(&format!("{name}/"))
                    .unwrap_or(&rule.target);
                output.push_str(&format!("target = {}\n", toml_string(target)));
                output.push_str(&format!("action = {}\n", toml_string(rule.action.as_str())));
                output.push_str(&format!("source = {}\n", toml_string(rule.source.as_str())));
                if let Some(reason) = &rule.reason {
                    output.push_str(&format!("reason = {}\n", toml_string(reason)));
                }
            }
            output.push('\n');
        }
        output
    }
}

fn provider_kind(provider: &ProviderConfig) -> &'static str {
    match provider {
        ProviderConfig::OpenAi(_) => "openai",
        ProviderConfig::Anthropic(_) => "anthropic",
        ProviderConfig::Google(_) => "google",
        ProviderConfig::AzureOpenAi(_) => "azure_openai",
        ProviderConfig::Bedrock(_) => "bedrock",
        ProviderConfig::Ollama(_) => "ollama",
        ProviderConfig::OpenAiCodex(_) => "openai_codex",
        ProviderConfig::GitHubCopilot(_) => "github_copilot",
        ProviderConfig::OpenAiCompatible(config) => config.preset.as_str(),
        ProviderConfig::Faux(_) => "faux",
    }
}

#[derive(Debug, Clone, Copy)]
struct ModelSamplingOptions {
    temperature: Option<f32>,
    top_p: Option<f32>,
    seed: Option<u64>,
    stop_set: bool,
    frequency_penalty: Option<f32>,
    presence_penalty: Option<f32>,
}

fn add_provider_model_warnings(
    provider: &ProviderConfig,
    model: &str,
    sampling: ModelSamplingOptions,
    max_output_tokens: Option<u32>,
    warnings: &mut Vec<ConfigWarning>,
) {
    warn_unsupported_model_options(provider, model, sampling, warnings);
    // M-58: Cerebras chat-completions v1 accepts `max_tokens` as an
    // alias today, but the v2 default-switchover (2026-07-21)
    // tightens schema validation to require `max_completion_tokens`.
    // Surface a soft warning at config-build time so operators
    // shipping pre-cutoff configs know they need to update before
    // the date rolls in; squeezy-llm's Cerebras path is responsible
    // for emitting `max_completion_tokens` on the wire from this
    // same `max_output_tokens`.
    if let ProviderConfig::OpenAiCompatible(compatible) = provider
        && compatible.preset == OpenAiCompatiblePreset::Cerebras
        && max_output_tokens.is_some()
    {
        warnings.push(ConfigWarning {
            source: "providers.cerebras".to_string(),
            field: "model.max_output_tokens emits `max_tokens` today; \
                    Cerebras v2 (default 2026-07-21) requires `max_completion_tokens`. \
                    squeezy-llm will switch wire keys automatically, but \
                    reasoning-model budgets count thinking tokens against \
                    the limit on v2."
                .to_string(),
        });
    }
}

fn is_generated_provider_model_warning(warning: &ConfigWarning) -> bool {
    warning.field.contains("Squeezy will omit it.")
        || warning
            .field
            .starts_with("model.max_output_tokens emits `max_tokens` today;")
}

fn warn_unsupported_model_options(
    provider: &ProviderConfig,
    model: &str,
    sampling: ModelSamplingOptions,
    warnings: &mut Vec<ConfigWarning>,
) {
    let support = model_option_support(provider, model);
    let provider_name = provider_kind(provider);
    let mut warn = |field: &'static str| {
        warnings.push(ConfigWarning {
            source: format!("providers.{provider_name}"),
            field: format!(
                "model.{field} is configured, but provider {provider_name} has no supported wire field for it; Squeezy will omit it."
            ),
        });
    };
    if sampling.temperature.is_some() && !support.temperature {
        warn("temperature");
    }
    if sampling.top_p.is_some() && !support.top_p {
        warn("top_p");
    }
    if sampling.seed.is_some() && !support.seed {
        warn("seed");
    }
    if sampling.stop_set && !support.stop {
        warn("stop");
    }
    if sampling.frequency_penalty.is_some() && !support.frequency_penalty {
        warn("frequency_penalty");
    }
    if sampling.presence_penalty.is_some() && !support.presence_penalty {
        warn("presence_penalty");
    }
}

#[derive(Debug, Clone, Copy)]
struct ModelOptionSupport {
    temperature: bool,
    top_p: bool,
    seed: bool,
    stop: bool,
    frequency_penalty: bool,
    presence_penalty: bool,
}

fn model_option_support(provider: &ProviderConfig, model: &str) -> ModelOptionSupport {
    let chat_completions = ModelOptionSupport {
        temperature: true,
        top_p: true,
        seed: true,
        stop: true,
        frequency_penalty: true,
        presence_penalty: true,
    };
    let openai_responses = ModelOptionSupport {
        temperature: true,
        top_p: true,
        seed: false,
        stop: false,
        frequency_penalty: false,
        presence_penalty: false,
    };
    let temp_top_stop = ModelOptionSupport {
        temperature: true,
        top_p: true,
        seed: false,
        stop: true,
        frequency_penalty: false,
        presence_penalty: false,
    };
    match provider {
        ProviderConfig::OpenAi(_)
        | ProviderConfig::AzureOpenAi(_)
        | ProviderConfig::OpenAiCodex(_) => openai_responses,
        ProviderConfig::Anthropic(_) | ProviderConfig::Google(_) | ProviderConfig::Bedrock(_) => {
            temp_top_stop
        }
        ProviderConfig::Ollama(_) => ModelOptionSupport {
            temperature: true,
            top_p: true,
            seed: true,
            stop: true,
            frequency_penalty: false,
            presence_penalty: false,
        },
        ProviderConfig::GitHubCopilot(_) => chat_completions,
        ProviderConfig::OpenAiCompatible(config)
            if config.preset == OpenAiCompatiblePreset::XAi =>
        {
            match classify_xai_route(model) {
                XaiRoute::Responses => openai_responses,
                XaiRoute::Chat => chat_completions,
                XaiRoute::ImageNotRouted => ModelOptionSupport {
                    temperature: false,
                    top_p: false,
                    seed: false,
                    stop: false,
                    frequency_penalty: false,
                    presence_penalty: false,
                },
            }
        }
        ProviderConfig::OpenAiCompatible(_) => chat_completions,
        ProviderConfig::Faux(_) => ModelOptionSupport {
            temperature: false,
            top_p: false,
            seed: false,
            stop: false,
            frequency_penalty: false,
            presence_penalty: false,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XaiRoute {
    Responses,
    Chat,
    ImageNotRouted,
}

fn classify_xai_route(model: &str) -> XaiRoute {
    let lower = model.to_ascii_lowercase();
    let id = lower.rsplit_once('/').map(|(_, id)| id).unwrap_or(&lower);
    if id.starts_with("grok-imagine") {
        return XaiRoute::ImageNotRouted;
    }
    if id.starts_with("grok-4") || id.starts_with("grok-build") || id.starts_with("grok-code") {
        return XaiRoute::Responses;
    }
    if matches_xai_grok_family(id, "grok-2")
        || matches_xai_grok_family(id, "grok-1")
        || id.starts_with("grok-beta")
    {
        return XaiRoute::Chat;
    }
    if id.starts_with("grok-") {
        return XaiRoute::Responses;
    }
    XaiRoute::Chat
}

fn matches_xai_grok_family(id: &str, family: &str) -> bool {
    id == family
        || id
            .strip_prefix(family)
            .is_some_and(|suffix| suffix.starts_with(['-', '.']))
}

/// Escape `value` as a TOML basic string. Public so persistence helpers in
/// downstream crates (e.g. permission rule writing) can share the same
/// escaping rules used by `config inspect`.
pub fn escape_toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn toml_string(value: &str) -> String {
    escape_toml_basic_string(value)
}

fn format_f32(value: f32) -> String {
    let mut formatted = format!("{value:.6}");
    while formatted.contains('.') && formatted.ends_with('0') {
        formatted.pop();
    }
    if formatted.ends_with('.') {
        formatted.push('0');
    }
    formatted
}

fn toml_string_array<S: AsRef<str>>(values: &[S]) -> String {
    let mut out = String::from("[");
    for (i, value) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&toml_string(value.as_ref()));
    }
    out.push(']');
    out
}

fn toml_path_array(values: &[PathBuf]) -> String {
    let values = values
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();
    toml_string_array(&values)
}

fn emit_skills_budget_mode(output: &mut String, key: &str, mode: SkillsBudgetMode) {
    match mode {
        SkillsBudgetMode::Chars { chars } => {
            output.push_str(&format!("{key} = {{ chars = {chars} }}\n"));
        }
        SkillsBudgetMode::ContextPercent { percent } => {
            // `{:?}` on f32 always emits a decimal point, keeping the TOML
            // parser on the float branch instead of misreading e.g. `2`
            // as an integer next round-trip.
            output.push_str(&format!(
                "{key} = {{ context_percent = {:?} }}\n",
                percent as f64
            ));
        }
    }
}

fn toml_bare_or_quoted_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-'))
    {
        key.to_string()
    } else {
        toml_string(key)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderConfig {
    OpenAi(OpenAiConfig),
    Anthropic(AnthropicConfig),
    Google(GoogleConfig),
    AzureOpenAi(AzureOpenAiConfig),
    Bedrock(BedrockConfig),
    Ollama(OllamaConfig),
    OpenAiCompatible(OpenAiCompatibleConfig),
    /// ChatGPT Plus/Pro subscription. The wire protocol is the OpenAI
    /// Responses API; auth is an OAuth access token persisted under
    /// `~/.squeezy/auth/openai-codex.json` rather than an env-var API
    /// key. The credential is never carried inline in the TOML — the
    /// settings only describe the endpoint and originator.
    OpenAiCodex(OpenAiCodexConfig),
    /// GitHub Copilot Chat subscription. Auth is a GitHub device-flow
    /// token persisted under `~/.squeezy/auth/github-copilot.json`;
    /// the request host is derived from the Copilot token at runtime.
    GitHubCopilot(GitHubCopilotConfig),
    /// Deterministic in-process faux provider for tests and the eval
    /// harness. The wire protocol is local: each `stream_response` call
    /// pops the next scripted response from an internal queue and replays
    /// it as a synthetic event stream. No outbound HTTP. See
    /// `squeezy-llm`'s `FauxProvider` for the runtime behaviour and
    /// script format.
    Faux(FauxConfig),
}

/// OpenAI Chat-Completions–style providers (one struct, many presets).
///
/// # Per-preset notes
///
/// **Fireworks (M-71)**: Fireworks ships three distinct API surfaces:
/// 1. `/v1/chat/completions` — the OpenAI-compatible chat shape that
///    this preset targets, supports the full Fireworks model catalog.
/// 2. `/v1/responses` — Fireworks' own Responses API with hosted MCP
///    tool support (not reachable via squeezy's `Fireworks` preset
///    today; users wanting MCP tools can wire the `Custom` preset to
///    `/v1/responses` and emit `OpenAI-Beta: responses=v1`).
/// 3. `/v1/messages` — Anthropic-compatible Messages API surface, used
///    by Pi for its 13 curated models. Reachable via the `Anthropic`
///    provider with a custom `base_url`.
///
/// **Cerebras (M-58)**: chat-completions v2 (default-switchover
/// 2026-07-21) tightens schema validation to require
/// `max_completion_tokens` instead of `max_tokens`. The squeezy-llm
/// Cerebras path emits the new key on the wire; squeezy-core surfaces
/// a `ConfigWarning` at build time when `model.max_output_tokens` is
/// set so operators can plan their cutover.
///
/// **Baseten (H-38)**: dedicated deployments live behind
/// per-deployment hosts (`https://model-{deployment_id}.api.baseten.co/...`).
/// Setting [`Self::deployment_id`] (or `BASETEN_DEPLOYMENT_ID`)
/// substitutes the placeholder before the request fires; the
/// `Baseten` preset still owns the `BASETEN_API_KEY` autoload and
/// the `baseten` registry namespace.
///
/// **Cloudflare AI Gateway (H-41)**: typed `cf-aig-*` knobs ride on
/// [`Self::cf_ai_gateway`]. User-supplied entries in
/// [`Self::extra_headers`] always win over the projected
/// `cf-aig-*` headers, matching the precedence for
/// `cf-aig-authorization`.
///
/// **Vertex (H-28, VX-A/B)**: opt in to OAuth-sourced bearer tokens
/// via [`Self::use_oauth`] (TOML `use_oauth = true` or implied by
/// `GOOGLE_APPLICATION_CREDENTIALS` without `VERTEX_ACCESS_TOKEN`).
/// The `global` location resolves to bare `aiplatform.googleapis.com`,
/// matching Google's only-via-global Gemini 3 routing.
///
/// **Vercel AI Gateway (VL-2)**: `VERCEL_OIDC_TOKEN` (12h TTL,
/// auto-injected into every Vercel function runtime) flows in as
/// `api_key_env` when no explicit `AI_GATEWAY_API_KEY` is set.
///
/// M-63: redaction applies to the serde `Serialize` path only; the derived
/// `Debug` prints the resolved `api_key` verbatim. Never `{:?}`-log an
/// `OpenAiCompatibleConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCompatibleConfig {
    pub preset: OpenAiCompatiblePreset,
    pub api_key_env: String,
    /// Inline plaintext API key resolved from the user/local TOML layer.
    /// `None` keeps the legacy env/keychain resolution. Never serialized
    /// in plain text — emits `"<redacted>"` for accidental dumps.
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    #[serde(serialize_with = "redact_secret_map")]
    pub extra_headers: BTreeMap<String, String>,
    pub transport: ProviderTransportConfig,
    /// Cloudflare account id. Populated for the Workers AI / AI Gateway
    /// presets so the LLM client can substitute the `{account_id}`
    /// placeholder in `base_url` before requests fire. `None` for every
    /// non-Cloudflare preset. `serde(default)` keeps older configs (and
    /// hand-rolled provider configs that predate this field) deserializing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Cloudflare AI Gateway id. Populated for the AI Gateway preset so
    /// `{gateway_id}` in `base_url` resolves before requests fire. `None`
    /// for the Workers AI preset and every non-Cloudflare preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_id: Option<String>,
    /// Baseten dedicated-deployment id. When set together with a
    /// `base_url` that carries `{deployment_id}` (or the per-deployment
    /// shape `https://model-{deployment_id}.api.baseten.co/environments/production/sync/v1`),
    /// the LLM client substitutes the placeholder before requests fire.
    /// Lets users pin SLA-bound or bring-your-own-checkpoint models
    /// without downgrading to the `Custom` preset and losing
    /// `BASETEN_API_KEY` autoload, the `baseten` provider label, and
    /// the `baseten` model-alias namespace. `None` keeps the existing
    /// shared-endpoint contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    /// Cloudflare AI Gateway typed knobs forwarded as `cf-aig-*`
    /// request headers. Populated when the active preset is
    /// [`OpenAiCompatiblePreset::CloudflareAiGateway`]; `None` for
    /// every other preset (so the schema cost is zero for them).
    /// User-supplied entries in [`Self::extra_headers`] always win
    /// over the values projected from this struct, matching the
    /// existing precedence for `cf-aig-authorization`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_ai_gateway: Option<CloudflareAiGatewayConfig>,
    /// Vertex AI: opt in to a refreshing OAuth source instead of the
    /// static `VERTEX_ACCESS_TOKEN` snapshot. The LLM client decides
    /// how to source tokens (squeezy-core does not depend on
    /// `squeezy-llm`'s oauth module); it can construct
    /// `VertexOAuthSource` from `GOOGLE_APPLICATION_CREDENTIALS` or
    /// `gcloud auth application-default print-access-token`. Default
    /// is `false`, preserving the historical static-snapshot
    /// behavior.
    #[serde(default, skip_serializing_if = "is_default_bool")]
    pub use_oauth: bool,
}

/// Typed `cf-aig-*` knob surface for the Cloudflare AI Gateway preset.
/// Each field maps 1:1 to a documented request header that the LLM
/// client emits at request time (header emission itself lives in
/// `squeezy-llm::compatible` so the schema layer stays
/// transport-agnostic). All fields are optional — leaving them unset
/// preserves the gateway's defaults. See Cloudflare's "AI Gateway
/// configuration headers" reference for the live list; squeezy
/// exposes the stable subset that covers caching, observability, and
/// per-request cost overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloudflareAiGatewayConfig {
    /// `cf-aig-cache-ttl` — overrides the gateway-configured cache
    /// TTL (seconds). Values are clamped by Cloudflare's bounds
    /// (60s – 1mo); squeezy passes the integer through so a new
    /// upper bound lands without a client release.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_ttl: Option<u32>,
    /// `cf-aig-skip-cache: true` — bypasses the gateway cache for
    /// this request. Useful for refresh probes and load tests.
    #[serde(default, skip_serializing_if = "is_default_bool")]
    pub skip_cache: bool,
    /// `cf-aig-event-id` — correlates this request with an upstream
    /// event in the operator's tracing system. Free-form string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// `cf-aig-step` — names the pipeline step (e.g. `"plan"` /
    /// `"act"`) so multi-step workflows can be filtered in the
    /// AI Gateway log explorer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// `cf-aig-collect-log: true` — opts into the gateway's
    /// per-request log capture. Default is the gateway's
    /// configured policy.
    #[serde(default, skip_serializing_if = "is_default_bool")]
    pub collect_log: bool,
    /// `cf-aig-skip-log: true` — opts the request out of logging
    /// (e.g. for PII-bearing inputs). Wins over [`Self::collect_log`]
    /// when both are set; Cloudflare also enforces this precedence.
    #[serde(default, skip_serializing_if = "is_default_bool")]
    pub skip_log: bool,
    /// `cf-aig-metadata` — JSON-stringified blob attached to the
    /// gateway log entry. Carry-through string; squeezy does not
    /// validate JSON syntax so newly-added fields land without
    /// migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    /// `cf-aig-cache-key` — overrides the cache key derivation so
    /// callers can dedupe requests across cosmetically-different
    /// prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
}

fn is_default_bool(value: &bool) -> bool {
    !*value
}

/// Named presets for the OpenAI-compatible (Chat Completions) provider. Each
/// preset carries enough defaults that the user can wire a provider with just
/// an API key. `Custom` is for any other OpenAI-compatible endpoint (e.g.
/// self-hosted LiteLLM, Cohere) and requires the caller to supply `base_url`
/// and `api_key_env` explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiCompatiblePreset {
    OpenRouter,
    Vercel,
    PortKey,
    Groq,
    XAi,
    DeepSeek,
    Vertex,
    Mistral,
    Together,
    Fireworks,
    Cerebras,
    DeepInfra,
    Baseten,
    // Serde's snake_case derivation chops between each upper/lower transition
    // (`LMStudio` -> `l_m_studio`), which would diverge from the TOML section
    // name. Pin the wire format explicitly to keep round-trip stable.
    #[serde(rename = "lmstudio")]
    LMStudio,
    #[serde(rename = "vllm")]
    VLlm,
    #[serde(rename = "llamacpp")]
    LlamaCpp,
    CloudflareWorkersAi,
    CloudflareAiGateway,
    Custom,
}

impl OpenAiCompatiblePreset {
    /// Kebab/snake-case identifier used in TOML provider section names, CLI
    /// `--provider` values, and the model registry.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::Vercel => "vercel",
            Self::PortKey => "portkey",
            Self::Groq => "groq",
            Self::XAi => "xai",
            Self::DeepSeek => "deepseek",
            Self::Vertex => "vertex",
            Self::Mistral => "mistral",
            Self::Together => "together",
            Self::Fireworks => "fireworks",
            Self::Cerebras => "cerebras",
            Self::DeepInfra => "deepinfra",
            Self::Baseten => "baseten",
            Self::LMStudio => "lmstudio",
            Self::VLlm => "vllm",
            Self::LlamaCpp => "llamacpp",
            Self::CloudflareWorkersAi => "cloudflare_workers_ai",
            Self::CloudflareAiGateway => "cloudflare_ai_gateway",
            Self::Custom => "openai_compatible",
        }
    }

    /// Human-readable label for the startup picker and `--list-providers`.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::Vercel => "Vercel AI Gateway",
            Self::PortKey => "PortKey",
            Self::Groq => "Groq",
            Self::XAi => "xAI",
            Self::DeepSeek => "DeepSeek",
            Self::Vertex => "Google Vertex AI",
            Self::Mistral => "Mistral AI",
            Self::Together => "Together AI",
            Self::Fireworks => "Fireworks AI",
            Self::Cerebras => "Cerebras",
            Self::DeepInfra => "DeepInfra",
            Self::Baseten => "Baseten",
            Self::LMStudio => "LM Studio",
            Self::VLlm => "vLLM",
            Self::LlamaCpp => "llama.cpp server",
            Self::CloudflareWorkersAi => "Cloudflare Workers AI",
            Self::CloudflareAiGateway => "Cloudflare AI Gateway",
            Self::Custom => "OpenAI-compatible (custom)",
        }
    }

    /// `true` when curated models exist in the registry and a dedicated costly
    /// integration test ships in `crates/squeezy-llm/tests/`. Light presets
    /// return `false` and fall back to generic context-window estimates.
    pub const fn is_full_tier(self) -> bool {
        // PortKey was historically full-tier but has no dedicated costly
        // integration test and routes via user-defined virtual keys /
        // integration slugs (see preset-portkey.md PK-3). Treat as a light
        // preset until a costly test ships.
        matches!(
            self,
            Self::OpenRouter
                | Self::Vercel
                | Self::Groq
                | Self::XAi
                | Self::DeepSeek
                | Self::Vertex
        )
    }

    pub const fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenRouter => DEFAULT_OPENROUTER_BASE_URL,
            Self::Vercel => DEFAULT_VERCEL_AI_BASE_URL,
            Self::PortKey => DEFAULT_PORTKEY_BASE_URL,
            Self::Groq => DEFAULT_GROQ_BASE_URL,
            Self::XAi => DEFAULT_XAI_BASE_URL,
            Self::DeepSeek => DEFAULT_DEEPSEEK_BASE_URL,
            // Vertex's base URL is per-project and per-region. The caller
            // must template it from `vertex_project` + `vertex_location`
            // (see `vertex_base_url`); presetting a static URL here would
            // hard-code one project.
            Self::Vertex => "",
            Self::Mistral => DEFAULT_MISTRAL_BASE_URL,
            Self::Together => DEFAULT_TOGETHER_BASE_URL,
            Self::Fireworks => DEFAULT_FIREWORKS_BASE_URL,
            Self::Cerebras => DEFAULT_CEREBRAS_BASE_URL,
            Self::DeepInfra => DEFAULT_DEEPINFRA_BASE_URL,
            Self::Baseten => DEFAULT_BASETEN_BASE_URL,
            Self::LMStudio => DEFAULT_LMSTUDIO_BASE_URL,
            Self::VLlm => DEFAULT_VLLM_BASE_URL,
            Self::LlamaCpp => DEFAULT_LLAMACPP_BASE_URL,
            // Cloudflare's base URLs are per-account (and per-gateway for the
            // gateway preset). The default templates carry `{account_id}`
            // and `{gateway_id}` placeholders that the LLM client substitutes
            // out of `OpenAiCompatibleConfig.account_id` / `.gateway_id`
            // before requests fire (see `substitute_url_placeholders` in
            // `squeezy-llm::compatible`). Users who override `base_url`
            // can keep the same placeholder syntax to route through a
            // custom reverse proxy without re-implementing the substitution.
            Self::CloudflareWorkersAi => DEFAULT_CLOUDFLARE_WORKERS_AI_BASE_URL,
            Self::CloudflareAiGateway => DEFAULT_CLOUDFLARE_AI_GATEWAY_BASE_URL,
            Self::Custom => "",
        }
    }

    pub const fn default_api_key_env(self) -> &'static str {
        match self {
            Self::OpenRouter => "OPENROUTER_API_KEY",
            Self::Vercel => "AI_GATEWAY_API_KEY",
            Self::PortKey => "PORTKEY_API_KEY",
            Self::Groq => "GROQ_API_KEY",
            Self::XAi => "XAI_API_KEY",
            Self::DeepSeek => "DEEPSEEK_API_KEY",
            // Vertex's "key" is an OAuth2 access token (~1 hour TTL). Users
            // either set this env var to a token they refresh themselves
            // (e.g. via `gcloud auth print-access-token`) or wire in a
            // service-account JSON helper.
            Self::Vertex => "VERTEX_ACCESS_TOKEN",
            Self::Mistral => "MISTRAL_API_KEY",
            Self::Together => "TOGETHER_API_KEY",
            Self::Fireworks => "FIREWORKS_API_KEY",
            Self::Cerebras => "CEREBRAS_API_KEY",
            Self::DeepInfra => "DEEPINFRA_API_KEY",
            Self::Baseten => "BASETEN_API_KEY",
            // Local self-hosted servers usually do not authenticate, but the env
            // var slot lets users layer a bearer token via a reverse proxy.
            Self::LMStudio => "LMSTUDIO_API_KEY",
            Self::VLlm => "VLLM_API_KEY",
            Self::LlamaCpp => "LLAMACPP_API_KEY",
            // Cloudflare uses one API token (CLOUDFLARE_API_KEY) for the
            // direct Workers AI endpoint, and the same token for the
            // upstream bearer when routing through AI Gateway. The optional
            // gateway-level token (`cf-aig-authorization`) is configured
            // separately via `CF_AIG_TOKEN` and injected as an extra header.
            // Vendor-canonical alias `CLOUDFLARE_API_TOKEN` is exposed via
            // the free fn [`preset_api_key_env_aliases`] (the single source
            // of truth for fallback env-var names).
            Self::CloudflareWorkersAi => "CLOUDFLARE_API_KEY",
            Self::CloudflareAiGateway => "CLOUDFLARE_API_KEY",
            Self::Custom => "",
        }
    }

    pub const fn default_model(self) -> &'static str {
        match self {
            Self::OpenRouter => DEFAULT_OPENROUTER_MODEL,
            Self::Vercel => DEFAULT_VERCEL_AI_MODEL,
            Self::PortKey => DEFAULT_PORTKEY_MODEL,
            Self::Groq => DEFAULT_GROQ_MODEL,
            Self::XAi => DEFAULT_XAI_MODEL,
            Self::DeepSeek => DEFAULT_DEEPSEEK_MODEL,
            Self::Vertex => DEFAULT_VERTEX_MODEL,
            Self::Mistral => DEFAULT_MISTRAL_MODEL,
            Self::Together => DEFAULT_TOGETHER_MODEL,
            Self::Fireworks => DEFAULT_FIREWORKS_MODEL,
            Self::Cerebras => DEFAULT_CEREBRAS_MODEL,
            Self::DeepInfra => DEFAULT_DEEPINFRA_MODEL,
            Self::Baseten => DEFAULT_BASETEN_MODEL,
            // Local servers expose whatever checkpoint the operator loaded; we
            // cannot guess a default, so the user supplies `model = …`.
            Self::LMStudio => "",
            Self::VLlm => "",
            Self::LlamaCpp => "",
            Self::CloudflareWorkersAi => DEFAULT_CLOUDFLARE_WORKERS_AI_MODEL,
            Self::CloudflareAiGateway => DEFAULT_CLOUDFLARE_AI_GATEWAY_MODEL,
            Self::Custom => "",
        }
    }

    /// Aliases accepted from CLI `--provider`, env `SQUEEZY_PROVIDER`, and TOML
    /// `model.provider`. The canonical name (`as_str`) is always accepted.
    pub fn parse(value: &str) -> Option<Self> {
        let normalised = value.trim().to_ascii_lowercase().replace('-', "_");
        match normalised.as_str() {
            "openrouter" | "open_router" => Some(Self::OpenRouter),
            "vercel" | "vercel_ai" | "vercel_ai_gateway" => Some(Self::Vercel),
            "portkey" | "port_key" => Some(Self::PortKey),
            "groq" => Some(Self::Groq),
            "xai" | "x_ai" | "grok" => Some(Self::XAi),
            "deepseek" | "deep_seek" => Some(Self::DeepSeek),
            "vertex" | "vertex_ai" | "google_vertex" | "google_vertex_ai" => Some(Self::Vertex),
            "mistral" | "mistral_ai" => Some(Self::Mistral),
            "together" | "together_ai" => Some(Self::Together),
            "fireworks" | "fireworks_ai" => Some(Self::Fireworks),
            "cerebras" => Some(Self::Cerebras),
            "deepinfra" | "deep_infra" => Some(Self::DeepInfra),
            "baseten" => Some(Self::Baseten),
            "lmstudio" | "lm_studio" => Some(Self::LMStudio),
            "vllm" => Some(Self::VLlm),
            "llamacpp" | "llama_cpp" | "llama_cpp_server" => Some(Self::LlamaCpp),
            "cloudflare_workers_ai" | "cloudflare_workersai" | "workers_ai" | "cf_workers_ai" => {
                Some(Self::CloudflareWorkersAi)
            }
            "cloudflare_ai_gateway" | "cloudflare_gateway" | "cf_ai_gateway" | "ai_gateway" => {
                Some(Self::CloudflareAiGateway)
            }
            "openai_compatible" | "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    /// Every preset that ships with `cargo run -p squeezy -- --list-providers`.
    /// Used by the CLI to enumerate options without hard-coding the list.
    pub fn all() -> [Self; 19] {
        [
            Self::OpenRouter,
            Self::Vercel,
            Self::PortKey,
            Self::Groq,
            Self::XAi,
            Self::DeepSeek,
            Self::Vertex,
            Self::Mistral,
            Self::Together,
            Self::Fireworks,
            Self::Cerebras,
            Self::DeepInfra,
            Self::Baseten,
            Self::LMStudio,
            Self::VLlm,
            Self::LlamaCpp,
            Self::CloudflareWorkersAi,
            Self::CloudflareAiGateway,
            Self::Custom,
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiConfig {
    pub api_key_env: String,
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    /// Pay-As-You-Go org slug forwarded as the `OpenAI-Organization`
    /// header so spend attributes against the right billing org when
    /// the API key has access to multiple. `None` keeps the legacy
    /// behavior of letting OpenAI pick the user's default org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Project id forwarded as `OpenAI-Project` so multi-project orgs
    /// attribute usage to the right project rather than the org's
    /// fallback project. Reads `OPENAI_PROJECT_ID` env / `project` TOML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Service tier forwarded as the `service_tier` body field on
    /// `/responses` and chat-completions calls. Accepts `"auto"`,
    /// `"default"`, `"flex"`, `"priority"`, and `"scale"`; the
    /// provider passes the string through so newly-added tiers do
    /// not require a client release. `None` lets OpenAI pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    pub transport: ProviderTransportConfig,
}

/// ChatGPT Plus/Pro subscription provider settings. The OAuth token
/// itself lives outside the TOML (under `~/.squeezy/auth/openai-codex.json`
/// with `chmod 600`); only the endpoint and the originator label are
/// configurable here. `base_url` accepts user overrides for testing
/// against a captive backend but defaults to the live ChatGPT Codex
/// endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiCodexConfig {
    pub base_url: String,
    pub originator: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubCopilotConfig {
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicConfig {
    pub api_key_env: String,
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleConfig {
    pub api_key_env: String,
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    pub transport: ProviderTransportConfig,
}

// M-63: redaction applies to the serde `Serialize` path only; the derived
// `Debug` prints the resolved `api_key` verbatim. Never `{:?}`-log an
// `AzureOpenAiConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureOpenAiConfig {
    pub api_key_env: String,
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: String,
    pub api_version: String,
    /// Maps logical model ids the caller uses in `[model]` (e.g. `gpt-4o`)
    /// to the Azure-deployment name the resource actually exposes
    /// (e.g. `my-deployment-gpt-4o`). When the body's `model` field is
    /// built, the provider substitutes the mapped value so users can
    /// keep stable model ids in config even when the Azure deployment
    /// is renamed or differs between environments. An entry missing from
    /// the map is sent through verbatim, preserving the historical
    /// "deployment id is the model id" behavior.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deployment_name_map: BTreeMap<String, String>,
    /// Operator-controlled HTTP headers forwarded on every Azure OpenAI
    /// request. Lets users wire `Apim-Subscription-Key` for API Management
    /// fronted deployments, `x-ms-client-request-id` for correlation,
    /// `x-ms-region` pinning, or an explicit `Authorization: Bearer …`
    /// that overrides the default `api-key` header (Entra ID / managed
    /// identity flows resolved out-of-band). User-supplied headers always
    /// win over the provider's defaults so an override is honored
    /// verbatim. Keyed via the standard `[providers.azure_openai.headers]`
    /// TOML table, matching the OpenAI-compatible preset shape.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        serialize_with = "redact_secret_map"
    )]
    pub extra_headers: BTreeMap<String, String>,
    /// Switches the provider away from the default `api-key: …` header
    /// to `Authorization: Bearer …` so Microsoft Entra ID / managed
    /// identity flows can authenticate against Azure OpenAI. The actual
    /// bearer is sourced from `AZURE_OPENAI_BEARER_TOKEN` (an external
    /// refresher — `az account get-access-token`, IMDS, or a sidecar —
    /// keeps the env var current). Defaults to `false`, preserving the
    /// historical API-key behavior.
    #[serde(default)]
    pub use_entra_id: bool,
    /// Bearer token resolved from the environment when `use_entra_id`
    /// is set. Snapshot at config build time; treat as short-lived and
    /// arrange for the supplying process to refresh the env var before
    /// expiry. `None` means no token was available — the provider
    /// then surfaces an explicit error rather than silently falling
    /// back to the api-key path.
    #[serde(default, serialize_with = "redact_secret_opt")]
    pub entra_bearer_token: Option<String>,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BedrockConfig {
    pub region: String,
    pub base_url: Option<String>,
    /// Optional short-lived bearer token sourced from
    /// `AWS_BEARER_TOKEN_BEDROCK`. When present the provider routes
    /// requests through the Bedrock HTTP bearer-auth scheme instead of
    /// the standard AWS SigV4 credential chain — matching the
    /// long-term "Amazon Bedrock API keys" feature that AWS's other
    /// SDKs (boto3, JS, Java) auto-detect from the same env var. Kept
    /// out of serialized config dumps via `redact_secret_opt` so it
    /// never lands in TOML logs alongside the rest of `[providers.bedrock]`.
    #[serde(default, serialize_with = "redact_secret_opt")]
    pub bearer_token: Option<String>,
    /// Operator-defined cost-allocation tags forwarded on every
    /// `ConverseStream` invocation so AWS can group invocations by
    /// team/env/project in CloudWatch + Cost Explorer.
    /// (F16pi-bedrock-request-metadata-tags.)
    #[serde(default)]
    pub request_metadata: BTreeMap<String, String>,
    pub transport: ProviderTransportConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OllamaConfig {
    pub base_url: String,
    /// Wire route for `POST <model>` traffic. `Native` keeps the proprietary
    /// `/api/chat` NDJSON path (default; exposes `keep_alive` + `num_ctx`);
    /// `OpenAiCompatible` rewrites the request to `/v1/chat/completions` SSE
    /// so users with portable tooling can pin Ollama to a uniform contract.
    #[serde(default)]
    pub route_style: OllamaRoute,
    pub transport: ProviderTransportConfig,
}

/// Configuration for the in-process faux provider used by tests and the
/// eval harness. The provider is wired through [`ProviderConfig::Faux`]
/// so eval / integration tests can target it with a `[providers.faux]`
/// TOML section instead of plumbing a bespoke mock through every entry
/// point.
///
/// Example settings:
///
/// ```toml
/// [model]
/// provider = "faux"
///
/// [providers.faux]
/// # Optional path to a TOML script file. When omitted the provider
/// # starts empty and callers must push responses programmatically.
/// script = "tests/fixtures/faux-script.toml"
/// # Optional override for the provider name reported by
/// # `LlmProvider::name` (defaults to "faux").
/// default_model = "faux-1"
/// ```
///
/// `script` is read by `squeezy-llm`'s `FauxProvider::from_config`; see
/// that crate for the script schema (a list of `[[turn]]` entries with
/// `text` / `thinking` / `tool_calls` / `error` / token-usage fields).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FauxConfig {
    /// Path to a TOML file describing the scripted responses. Resolved
    /// relative to the process working directory when the provider is
    /// constructed.
    #[serde(default)]
    pub script: Option<String>,
    /// Optional override for the provider name returned by
    /// `LlmProvider::name`. Falls back to `"faux"` when unset.
    #[serde(default)]
    pub name: Option<String>,
    /// Retry / timeout knobs are accepted for surface symmetry with the
    /// real providers but ignored by the in-process faux implementation.
    #[serde(default)]
    pub transport: ProviderTransportConfig,
}

/// Selects which HTTP route the Ollama provider uses to stream completions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OllamaRoute {
    /// Ollama's native NDJSON endpoint (`<base>/chat`). Preserves Ollama
    /// extensions like `keep_alive` and `num_ctx` for local hardware tuning.
    #[default]
    Native,
    /// OpenAI-compatible Chat Completions SSE endpoint (`<root>/v1/chat/completions`).
    /// Portable wire shape; loses Ollama-specific options.
    OpenAiCompatible,
}

impl OllamaRoute {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "native" | "api" | "default" => Some(Self::Native),
            "openai_compatible" | "openai" | "v1" | "compat" => Some(Self::OpenAiCompatible),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderTransportConfig {
    pub request_max_retries: u8,
    pub stream_max_retries: u8,
    pub stream_idle_timeout_ms: u64,
    /// Idle timeout (ms) for TCP connections sitting in the shared
    /// HTTP pool. `0` disables eviction entirely (connections live
    /// until the remote closes them). Controls pool eviction;
    /// per-event SSE idle gating stays governed by
    /// [`Self::stream_idle_timeout_ms`].
    pub pool_idle_timeout_ms: u64,
    /// Maximum idle TCP connections kept per origin in the shared
    /// HTTP pool. `u32::MAX` is effectively unbounded.
    pub pool_max_idle_per_host: u32,
    /// Upper bound (ms) on any inter-retry sleep. The retry path
    /// honors `Retry-After` / `Retry-After-Ms` hints from the
    /// upstream, but clamps the resulting delay to this value so a
    /// malicious or buggy header (e.g. `Retry-After: 999999`) can't
    /// hang the agent indefinitely.
    pub max_retry_delay_ms: u64,
}

impl Default for ProviderTransportConfig {
    fn default() -> Self {
        Self {
            request_max_retries: DEFAULT_PROVIDER_REQUEST_MAX_RETRIES,
            stream_max_retries: DEFAULT_PROVIDER_STREAM_MAX_RETRIES,
            stream_idle_timeout_ms: DEFAULT_PROVIDER_STREAM_IDLE_TIMEOUT_MS,
            pool_idle_timeout_ms: DEFAULT_PROVIDER_POOL_IDLE_TIMEOUT_MS,
            pool_max_idle_per_host: DEFAULT_PROVIDER_POOL_MAX_IDLE_PER_HOST,
            max_retry_delay_ms: DEFAULT_PROVIDER_MAX_RETRY_DELAY_MS,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProfile {
    Cheap,
    #[default]
    Balanced,
    Strong,
}

impl ModelProfile {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "cheap" => Some(Self::Cheap),
            "balanced" | "default" => Some(Self::Balanced),
            "strong" => Some(Self::Strong),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cheap => "cheap",
            Self::Balanced => "balanced",
            Self::Strong => "strong",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh")]
    XHigh,
}

impl ReasoningEffort {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "x_high" => Some(Self::XHigh),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }

    /// Anthropic-style thinking budget in tokens for this effort level.
    pub const fn thinking_budget_tokens(self) -> u32 {
        match self {
            Self::Low => 4_096,
            Self::Medium => 16_384,
            Self::High => 32_768,
            Self::XHigh => 60_000,
        }
    }
}

/// One `[model_limits."<provider>:<model>"]` entry — the per-model context
/// window override. Today it carries only the window; kept as a table (not a
/// bare int) so future per-model limit knobs slot in without a format change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLimitOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

impl ModelLimitOverride {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["context_window"], source, path)?;
        Ok(Self {
            context_window: u64_value(
                table,
                "context_window",
                source,
                &field(path, "context_window"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.context_window, next.context_window);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SettingsFile {
    pub provider: Option<String>,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub model_settings: Option<ModelSettings>,
    pub providers: Option<BTreeMap<String, ProviderSettings>>,
    pub model_limits: Option<BTreeMap<String, ModelLimitOverride>>,
    pub agent: Option<AgentSettings>,
    pub session: Option<SessionSettings>,
    pub context: Option<ContextCompactionSettings>,
    pub subagents: Option<SubagentSettings>,
    pub budgets: Option<BudgetSettings>,
    pub routing: Option<RoutingSettings>,
    pub permissions: Option<PermissionSettings>,
    pub telemetry: Option<TelemetrySettings>,
    pub feedback: Option<FeedbackSettings>,
    pub redaction: Option<RedactionSettings>,
    pub web: Option<WebSettings>,
    pub skills: Option<SkillsSettings>,
    pub graph: Option<GraphSettings>,
    pub cache: Option<CacheSettings>,
    pub tools: Option<ToolSchemaSettings>,
    pub tui: Option<TuiSettings>,
    pub mcp: Option<McpSettings>,
    pub hardening: Option<HardeningSettings>,
    /// Named configuration profiles. A `[profiles.<name>]` TOML section
    /// accepts the same leaves as the root settings file and is merged on
    /// top of the base settings when the user selects `<name>` via
    /// `--profile`. The `profiles` and `profile` leaves of the inner table
    /// are stripped so a profile cannot recursively select another profile.
    pub profiles: Option<BTreeMap<String, SettingsFile>>,
}

impl SettingsFile {
    pub fn load_optional(path: &Path) -> Result<Self> {
        Ok(Self::load_optional_source(path, "settings")?.0)
    }

    fn load_optional_source(
        path: &Path,
        label: &str,
    ) -> Result<(Self, Vec<String>, Vec<ConfigWarning>)> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((Self::default(), vec!["defaults".to_string()], Vec::new()));
            }
            Err(error) => return Err(error.into()),
        };
        let source = format!("{label}:{}", path.display());
        let settings = Self::from_toml_str(&text, &source)?;
        let unknowns = take_unknown_fields();
        Ok((
            settings,
            vec!["defaults".to_string(), source.clone()],
            config_warnings_from_unknown_fields(&source, unknowns),
        ))
    }

    pub fn from_toml_str(text: &str, source: &str) -> Result<Self> {
        UNKNOWN_FIELDS.with(|cell| cell.borrow_mut().clear());
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let table = toml::from_str::<toml::value::Table>(text)
            .map_err(|err| SqueezyError::Config(format!("{source}: {err}")))?;
        Self::from_toml_table(&table, source)
    }

    fn from_toml_table(table: &toml::value::Table, source: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "provider",
                "profile",
                "model",
                "providers",
                "model_limits",
                "agent",
                "session",
                "context",
                "subagents",
                "budgets",
                "routing",
                "permissions",
                "telemetry",
                "feedback",
                "redaction",
                "web",
                "skills",
                "graph",
                "cache",
                "tools",
                "tui",
                "mcp",
                "hardening",
                "profiles",
            ],
            source,
            "",
        )?;

        let mut settings = Self {
            provider: string_value(table, "provider", source, "provider")?,
            profile: string_value(table, "profile", source, "profile")?,
            ..Self::default()
        };
        if let Some(value) = table.get("model") {
            if let Some(model) = value.as_str() {
                settings.model = Some(model.to_string());
            } else if let Some(model_table) = value.as_table() {
                settings.model_settings =
                    Some(ModelSettings::from_table(model_table, source, "model")?);
            } else {
                return Err(type_error(source, "model", "string or table"));
            }
        }
        settings.providers = providers_settings(table, source)?;
        settings.model_limits = model_limits_settings(table, source)?;
        settings.agent = optional_table(table, "agent", source)?
            .map(|table| AgentSettings::from_table(table, source, "agent"))
            .transpose()?;
        settings.session = optional_table(table, "session", source)?
            .map(|table| SessionSettings::from_table(table, source, "session"))
            .transpose()?;
        settings.context = optional_table(table, "context", source)?
            .map(|table| ContextCompactionSettings::from_table(table, source, "context"))
            .transpose()?;
        settings.subagents = optional_table(table, "subagents", source)?
            .map(|table| SubagentSettings::from_table(table, source, "subagents"))
            .transpose()?;
        settings.budgets = optional_table(table, "budgets", source)?
            .map(|table| BudgetSettings::from_table(table, source, "budgets"))
            .transpose()?;
        settings.routing = optional_table(table, "routing", source)?
            .map(|table| RoutingSettings::from_table(table, source, "routing"))
            .transpose()?;
        settings.permissions = optional_table(table, "permissions", source)?
            .map(|table| PermissionSettings::from_table(table, source, "permissions"))
            .transpose()?;
        settings.telemetry = optional_table(table, "telemetry", source)?
            .map(|table| TelemetrySettings::from_table(table, source, "telemetry"))
            .transpose()?;
        settings.feedback = optional_table(table, "feedback", source)?
            .map(|table| FeedbackSettings::from_table(table, source, "feedback"))
            .transpose()?;
        settings.redaction = optional_table(table, "redaction", source)?
            .map(|table| RedactionSettings::from_table(table, source, "redaction"))
            .transpose()?;
        settings.web = optional_table(table, "web", source)?
            .map(|table| WebSettings::from_table(table, source, "web"))
            .transpose()?;
        settings.skills = optional_table(table, "skills", source)?
            .map(|table| SkillsSettings::from_table(table, source, "skills"))
            .transpose()?;
        settings.graph = optional_table(table, "graph", source)?
            .map(|table| GraphSettings::from_table(table, source, "graph"))
            .transpose()?;
        settings.cache = optional_table(table, "cache", source)?
            .map(|table| CacheSettings::from_table(table, source, "cache"))
            .transpose()?;
        settings.tools = optional_table(table, "tools", source)?
            .map(|table| ToolSchemaSettings::from_table(table, source, "tools"))
            .transpose()?;
        settings.tui = optional_table(table, "tui", source)?
            .map(|table| TuiSettings::from_table(table, source, "tui"))
            .transpose()?;
        settings.mcp = optional_table(table, "mcp", source)?
            .map(|table| McpSettings::from_table(table, source, "mcp"))
            .transpose()?;
        settings.hardening = optional_table(table, "hardening", source)?
            .map(|table| HardeningSettings::from_table(table, source, "hardening"))
            .transpose()?;
        settings.profiles = parse_profiles_map(table, source)?;
        Ok(settings)
    }

    /// Merges the named `[profiles.<name>]` section on top of the base
    /// settings. Returns an error if `name` isn't present in `profiles`.
    /// After application, the `profiles` map is cleared so downstream
    /// merging and `inspect` paths don't re-emit it.
    pub fn apply_profile(&mut self, name: &str) -> Result<()> {
        let Some(profile_map) = self.profiles.take() else {
            return Err(SqueezyError::Config(format!(
                "profile {name:?} not found; no `[profiles.*]` sections are configured"
            )));
        };
        let Some(profile) = profile_map.get(name).cloned() else {
            let mut available: Vec<&String> = profile_map.keys().collect();
            available.sort();
            return Err(SqueezyError::Config(format!(
                "profile {name:?} not found; available profiles: {available:?}"
            )));
        };
        self.merge(profile);
        Ok(())
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.provider, next.provider);
        replace_if_some(&mut self.profile, next.profile);
        replace_if_some(&mut self.model, next.model);
        merge_option(
            &mut self.model_settings,
            next.model_settings,
            ModelSettings::merge,
        );
        merge_provider_maps(&mut self.providers, next.providers);
        merge_model_limit_maps(&mut self.model_limits, next.model_limits);
        merge_option(&mut self.agent, next.agent, AgentSettings::merge);
        merge_option(&mut self.session, next.session, SessionSettings::merge);
        merge_option(
            &mut self.context,
            next.context,
            ContextCompactionSettings::merge,
        );
        merge_option(&mut self.subagents, next.subagents, SubagentSettings::merge);
        merge_option(&mut self.budgets, next.budgets, BudgetSettings::merge);
        merge_option(&mut self.routing, next.routing, RoutingSettings::merge);
        merge_option(
            &mut self.permissions,
            next.permissions,
            PermissionSettings::merge,
        );
        merge_option(
            &mut self.telemetry,
            next.telemetry,
            TelemetrySettings::merge,
        );
        merge_option(&mut self.feedback, next.feedback, FeedbackSettings::merge);
        merge_option(
            &mut self.redaction,
            next.redaction,
            RedactionSettings::merge,
        );
        merge_option(&mut self.web, next.web, WebSettings::merge);
        merge_option(&mut self.skills, next.skills, SkillsSettings::merge);
        merge_option(&mut self.graph, next.graph, GraphSettings::merge);
        merge_option(&mut self.cache, next.cache, CacheSettings::merge);
        merge_option(&mut self.tools, next.tools, ToolSchemaSettings::merge);
        merge_option(&mut self.tui, next.tui, TuiSettings::merge);
        merge_option(&mut self.mcp, next.mcp, McpSettings::merge);
        merge_option(
            &mut self.hardening,
            next.hardening,
            HardeningSettings::merge,
        );
        merge_profiles_maps(&mut self.profiles, next.profiles);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct HardeningSettings {
    pub disable_core_dumps: Option<bool>,
    pub deny_debug_attach: Option<bool>,
}

impl HardeningSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &["disable_core_dumps", "deny_debug_attach"],
            source,
            path,
        )?;
        Ok(Self {
            disable_core_dumps: bool_value(
                table,
                "disable_core_dumps",
                source,
                &field(path, "disable_core_dumps"),
            )?,
            deny_debug_attach: bool_value(
                table,
                "deny_debug_attach",
                source,
                &field(path, "deny_debug_attach"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.disable_core_dumps, next.disable_core_dumps);
        replace_if_some(&mut self.deny_debug_attach, next.deny_debug_attach);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HardeningConfig {
    pub disable_core_dumps: bool,
    pub deny_debug_attach: bool,
}

impl Default for HardeningConfig {
    fn default() -> Self {
        Self {
            disable_core_dumps: true,
            deny_debug_attach: true,
        }
    }
}

impl HardeningConfig {
    fn from_settings(settings: HardeningSettings) -> Self {
        Self {
            disable_core_dumps: settings.disable_core_dumps.unwrap_or(true),
            deny_debug_attach: settings.deny_debug_attach.unwrap_or(true),
        }
    }
}

// M-63: redaction lives on the serde `Serialize` path only (see
// `redact_secret_opt` below). The derived `Debug` is NOT redacted — the
// inline `api_key` prints verbatim under `{:?}`. Never `{:?}`-log a
// `ProviderSettings`; serialize it (or its fields individually) instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderSettings {
    pub api_key_env: Option<String>,
    /// Inline plaintext API key, persisted in the user/local TOML layer.
    /// Serde Serialize emits `"<redacted>"` so accidental
    /// `to_string` / inspect paths can't leak it; Deserialize parses the
    /// real value out of TOML normally.
    #[serde(serialize_with = "redact_secret_opt")]
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub default_model: Option<String>,
    pub api_version: Option<String>,
    pub region: Option<String>,
    pub preset: Option<String>,
    pub vertex_project: Option<String>,
    pub vertex_location: Option<String>,
    pub cloudflare_account_id: Option<String>,
    pub cloudflare_gateway_id: Option<String>,
    pub request_max_retries: Option<u8>,
    pub stream_max_retries: Option<u8>,
    pub stream_idle_timeout_ms: Option<u64>,
    /// `[providers.<section>.headers]` carries `extra_headers` that
    /// reach the upstream verbatim. The Custom-preset workaround for
    /// non-Bearer auth (LiteLLM `x-litellm-key`, PortKey
    /// `x-portkey-api-key`, vLLM bearer, corporate proxies that want
    /// `api-key` / `x-api-key`) routes the actual secret through here,
    /// so M-63 masks each value as `"<redacted>"` on the Serialize
    /// side. The *names* of the headers stay visible so operators can
    /// see which keys are wired without leaking the values; the
    /// Deserialize side is untouched so loaded TOML still binds to the
    /// real values at request time.
    #[serde(serialize_with = "redact_secret_map_opt")]
    pub headers: Option<BTreeMap<String, String>>,
    /// Bedrock only: operator-defined cost-allocation tags threaded into
    /// every ConverseStream request via `set_request_metadata` so AWS
    /// can group invocations by team/env/project. Non-Bedrock providers
    /// ignore this setting.
    pub request_metadata: Option<BTreeMap<String, String>>,
    /// Azure OpenAI only: logical model id → Azure-deployment name. Keeps
    /// callers' `[model]` ids stable even when the deployment is renamed
    /// or differs between environments. See
    /// [`AzureOpenAiConfig::deployment_name_map`] for the runtime
    /// substitution contract.
    pub deployment_name_map: Option<BTreeMap<String, String>>,
    /// Ollama-only: `"native"` (default) or `"openai_compatible"` to pin the
    /// provider to `/v1/chat/completions` SSE instead of `/api/chat` NDJSON.
    pub route_style: Option<String>,
    /// Faux-only: path to a TOML script file consumed by the in-process
    /// faux provider. Other providers ignore this field.
    pub script: Option<String>,
    /// Azure-only: opt in to the Entra ID / managed-identity bearer auth
    /// path. When `Some(true)` the provider emits `Authorization: Bearer`
    /// sourced from `AZURE_OPENAI_BEARER_TOKEN` instead of the default
    /// `api-key` header. Tri-state (`None`) so the higher-precedence
    /// layer can leave the value untouched during settings merge.
    pub use_entra_id: Option<bool>,
    /// OpenAI-only: PayG org slug forwarded as `OpenAI-Organization`.
    /// Ignored by every other provider.
    pub organization: Option<String>,
    /// OpenAI-only: project id forwarded as `OpenAI-Project`. Ignored by
    /// every other provider.
    pub project: Option<String>,
    /// OpenAI-only: `service_tier` body field (`flex`, `priority`,
    /// `default`, `auto`, `scale`). Pass-through string so new tiers
    /// land without a client release. Ignored by every other provider.
    pub service_tier: Option<String>,
    /// Baseten-only: dedicated-deployment id used to substitute
    /// `{deployment_id}` in the resolved `base_url`. Ignored by every
    /// other preset.
    pub deployment_id: Option<String>,
    /// Cloudflare AI Gateway only: typed `cf-aig-*` knob surface.
    /// Stored as a tri-state nested table — `None` means the TOML
    /// section omitted the `[providers.cloudflare_ai_gateway.cf_ai_gateway]`
    /// block entirely, so the higher-precedence layer's value (if any)
    /// remains in effect during merge.
    pub cf_ai_gateway: Option<CloudflareAiGatewayConfig>,
    /// Vertex-only: opt in to OAuth-sourced bearer tokens. Tri-state
    /// (`None` to preserve the higher-precedence layer during merge,
    /// `Some(false)` to explicitly disable, `Some(true)` to enable).
    pub use_oauth: Option<bool>,
    /// Per-provider turn-routing overrides. Routing is provider-scoped — the
    /// cheap/judge models for `openai` make no sense under `anthropic` — so
    /// these live here rather than in the global `[routing]` table. Each falls
    /// back (per-provider → legacy global → built-in) when `None`; see
    /// `cheap_model_for` / `judge_model_for` in `squeezy-agent`.
    ///
    /// The model easy turns are rerouted to. `None` = the per-provider built-in
    /// (`small_fast_model_for_provider`).
    pub cheap_model: Option<String>,
    /// The model that classifies turns cheap-vs-parent. `None` = the
    /// per-provider built-in mini tier (`judge_model_for_provider`). Should be a
    /// cheap/fast model.
    pub judge_model: Option<String>,
    /// Custom judge instructions. `None` = the built-in per-provider prompt.
    pub judge_prompt: Option<String>,
    /// Reroute filter: a single case-insensitive regex selecting which parent
    /// models to reroute (a leading `!` excludes; combine with `|`). `None` =
    /// the built-in per-provider default (skip this provider's cheap tiers); an
    /// explicit empty string reroutes any model.
    pub expensive_models: Option<String>,
}

impl ProviderSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "api_key_env",
                "api_key",
                "base_url",
                "default_model",
                "api_version",
                "region",
                "preset",
                "vertex_project",
                "vertex_location",
                "cloudflare_account_id",
                "cloudflare_gateway_id",
                "request_max_retries",
                "stream_max_retries",
                "stream_idle_timeout_ms",
                "headers",
                "request_metadata",
                "deployment_name_map",
                "route_style",
                "script",
                "use_entra_id",
                "organization",
                "project",
                "service_tier",
                "deployment_id",
                "cf_ai_gateway",
                "use_oauth",
                "cheap_model",
                "judge_model",
                "judge_prompt",
                "expensive_models",
            ],
            source,
            path,
        )?;
        let headers = match table.get("headers") {
            None => None,
            Some(toml::Value::Table(table)) => {
                let mut map = BTreeMap::new();
                for (key, value) in table {
                    let header_path = field(path, &format!("headers.{key}"));
                    let toml::Value::String(value) = value else {
                        return Err(SqueezyError::Config(format!(
                            "{source}: {header_path} must map to string values"
                        )));
                    };
                    let resolved = resolve_shell_escape(value.clone(), source, &header_path)?;
                    // M-65: reject CR/LF (and any other non-`HeaderValue`
                    // byte) at config-load. `http::HeaderValue` already
                    // refuses anything outside `[0x20..0x7E] ∪ {0x09}`
                    // at request time, but the failure mode there is a
                    // deferred reqwest builder error ("invalid HTTP
                    // header value") with no field name — the operator
                    // sees the error mid-stream and has to guess which
                    // header tripped it. Failing here points at the
                    // exact TOML path and notes that CR/LF are forbidden
                    // so a copy-paste-from-curl mishap surfaces with a
                    // usable hint. Header *names* are not validated
                    // here: `reqwest` already rejects bad names at
                    // request-construction time and the audit (M-65)
                    // explicitly scopes this check to values only.
                    if http::HeaderValue::from_str(&resolved).is_err() {
                        return Err(SqueezyError::Config(format!(
                            "{source}: {header_path} contains bytes that cannot be sent as an \
                             HTTP header value (CR/LF and other control characters are \
                             forbidden); strip them or escape them out of band"
                        )));
                    }
                    map.insert(key.clone(), resolved);
                }
                Some(map)
            }
            Some(_) => {
                return Err(SqueezyError::Config(format!(
                    "{source}: {} must be a TOML table of string values",
                    field(path, "headers"),
                )));
            }
        };
        let deployment_name_map = match table.get("deployment_name_map") {
            None => None,
            Some(toml::Value::Table(table)) => {
                let mut map = BTreeMap::new();
                for (key, value) in table {
                    let entry_path = field(path, &format!("deployment_name_map.{key}"));
                    let toml::Value::String(value) = value else {
                        return Err(SqueezyError::Config(format!(
                            "{source}: {entry_path} must map to string deployment names"
                        )));
                    };
                    map.insert(key.clone(), value.clone());
                }
                Some(map)
            }
            Some(_) => {
                return Err(SqueezyError::Config(format!(
                    "{source}: {} must be a TOML table of string deployment names",
                    field(path, "deployment_name_map"),
                )));
            }
        };
        let cf_ai_gateway = match table.get("cf_ai_gateway") {
            None => None,
            Some(toml::Value::Table(inner)) => {
                let inner_path = field(path, "cf_ai_gateway");
                reject_unknown_keys(
                    inner,
                    &[
                        "cache_ttl",
                        "skip_cache",
                        "event_id",
                        "step",
                        "collect_log",
                        "skip_log",
                        "metadata",
                        "cache_key",
                    ],
                    source,
                    &inner_path,
                )?;
                Some(CloudflareAiGatewayConfig {
                    cache_ttl: match u64_nonnegative_value(
                        inner,
                        "cache_ttl",
                        source,
                        &field(&inner_path, "cache_ttl"),
                    )? {
                        None => None,
                        Some(value) if value <= u32::MAX as u64 => Some(value as u32),
                        Some(value) => {
                            return Err(SqueezyError::Config(format!(
                                "{source}: {}: expected an integer fitting in u32 (got {value})",
                                field(&inner_path, "cache_ttl"),
                            )));
                        }
                    },
                    skip_cache: bool_value(
                        inner,
                        "skip_cache",
                        source,
                        &field(&inner_path, "skip_cache"),
                    )?
                    .unwrap_or(false),
                    event_id: string_value(
                        inner,
                        "event_id",
                        source,
                        &field(&inner_path, "event_id"),
                    )?,
                    step: string_value(inner, "step", source, &field(&inner_path, "step"))?,
                    collect_log: bool_value(
                        inner,
                        "collect_log",
                        source,
                        &field(&inner_path, "collect_log"),
                    )?
                    .unwrap_or(false),
                    skip_log: bool_value(
                        inner,
                        "skip_log",
                        source,
                        &field(&inner_path, "skip_log"),
                    )?
                    .unwrap_or(false),
                    metadata: string_value(
                        inner,
                        "metadata",
                        source,
                        &field(&inner_path, "metadata"),
                    )?,
                    cache_key: string_value(
                        inner,
                        "cache_key",
                        source,
                        &field(&inner_path, "cache_key"),
                    )?,
                })
            }
            Some(_) => {
                return Err(SqueezyError::Config(format!(
                    "{source}: {} must be a TOML table of cf-aig-* knobs",
                    field(path, "cf_ai_gateway"),
                )));
            }
        };
        Ok(Self {
            api_key_env: string_value(table, "api_key_env", source, &field(path, "api_key_env"))?,
            api_key: string_value(table, "api_key", source, &field(path, "api_key"))?,
            base_url: string_value(table, "base_url", source, &field(path, "base_url"))?,
            default_model: string_value(
                table,
                "default_model",
                source,
                &field(path, "default_model"),
            )?,
            api_version: string_value(table, "api_version", source, &field(path, "api_version"))?,
            region: string_value(table, "region", source, &field(path, "region"))?,
            preset: string_value(table, "preset", source, &field(path, "preset"))?,
            vertex_project: string_value(
                table,
                "vertex_project",
                source,
                &field(path, "vertex_project"),
            )?,
            vertex_location: string_value(
                table,
                "vertex_location",
                source,
                &field(path, "vertex_location"),
            )?,
            cloudflare_account_id: string_value(
                table,
                "cloudflare_account_id",
                source,
                &field(path, "cloudflare_account_id"),
            )?,
            cloudflare_gateway_id: string_value(
                table,
                "cloudflare_gateway_id",
                source,
                &field(path, "cloudflare_gateway_id"),
            )?,
            request_max_retries: u8_nonnegative_value(
                table,
                "request_max_retries",
                source,
                &field(path, "request_max_retries"),
            )?,
            stream_max_retries: u8_nonnegative_value(
                table,
                "stream_max_retries",
                source,
                &field(path, "stream_max_retries"),
            )?,
            stream_idle_timeout_ms: u64_nonnegative_value(
                table,
                "stream_idle_timeout_ms",
                source,
                &field(path, "stream_idle_timeout_ms"),
            )?,
            headers,
            request_metadata: None,
            deployment_name_map,
            route_style: string_value(table, "route_style", source, &field(path, "route_style"))?,
            script: string_value(table, "script", source, &field(path, "script"))?,
            use_entra_id: bool_value(table, "use_entra_id", source, &field(path, "use_entra_id"))?,
            organization: string_value(
                table,
                "organization",
                source,
                &field(path, "organization"),
            )?,
            project: string_value(table, "project", source, &field(path, "project"))?,
            service_tier: string_value(
                table,
                "service_tier",
                source,
                &field(path, "service_tier"),
            )?,
            deployment_id: string_value(
                table,
                "deployment_id",
                source,
                &field(path, "deployment_id"),
            )?,
            cf_ai_gateway,
            use_oauth: bool_value(table, "use_oauth", source, &field(path, "use_oauth"))?,
            cheap_model: string_value(table, "cheap_model", source, &field(path, "cheap_model"))?,
            judge_model: string_value(table, "judge_model", source, &field(path, "judge_model"))?,
            judge_prompt: string_value(
                table,
                "judge_prompt",
                source,
                &field(path, "judge_prompt"),
            )?,
            expensive_models: string_value(
                table,
                "expensive_models",
                source,
                &field(path, "expensive_models"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.api_key_env, next.api_key_env);
        replace_if_some(&mut self.api_key, next.api_key);
        replace_if_some(&mut self.base_url, next.base_url);
        replace_if_some(&mut self.default_model, next.default_model);
        replace_if_some(&mut self.api_version, next.api_version);
        replace_if_some(&mut self.region, next.region);
        replace_if_some(&mut self.preset, next.preset);
        replace_if_some(&mut self.vertex_project, next.vertex_project);
        replace_if_some(&mut self.vertex_location, next.vertex_location);
        replace_if_some(&mut self.cloudflare_account_id, next.cloudflare_account_id);
        replace_if_some(&mut self.cloudflare_gateway_id, next.cloudflare_gateway_id);
        replace_if_some(&mut self.request_max_retries, next.request_max_retries);
        replace_if_some(&mut self.stream_max_retries, next.stream_max_retries);
        replace_if_some(
            &mut self.stream_idle_timeout_ms,
            next.stream_idle_timeout_ms,
        );
        replace_if_some(&mut self.headers, next.headers);
        replace_if_some(&mut self.route_style, next.route_style);
        replace_if_some(&mut self.script, next.script);
        replace_if_some(&mut self.use_entra_id, next.use_entra_id);
        replace_if_some(&mut self.organization, next.organization);
        replace_if_some(&mut self.project, next.project);
        replace_if_some(&mut self.service_tier, next.service_tier);
        replace_if_some(&mut self.deployment_id, next.deployment_id);
        replace_if_some(&mut self.cf_ai_gateway, next.cf_ai_gateway);
        replace_if_some(&mut self.use_oauth, next.use_oauth);
        replace_if_some(&mut self.cheap_model, next.cheap_model);
        replace_if_some(&mut self.judge_model, next.judge_model);
        replace_if_some(&mut self.judge_prompt, next.judge_prompt);
        replace_if_some(&mut self.expensive_models, next.expensive_models);
    }
}

/// Serde Serializer hook for `Option<String>` fields holding a secret.
/// `None` round-trips as null/absent; `Some(_)` emits the literal
/// `"<redacted>"` so any path that serializes the struct (debug dumps,
/// inspect output, accidental `to_string`) can't leak the plaintext.
fn redact_secret_opt<S>(
    value: &Option<String>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match value {
        Some(_) => serializer.serialize_some("<redacted>"),
        None => serializer.serialize_none(),
    }
}

/// Serialize-side redactor for header maps. M-63: a `Custom`-preset
/// workaround for non-Bearer auth is to smuggle the secret through
/// `[providers.openai_compatible.headers] x-api-key = "..."`, but
/// `ProviderSettings::headers` would otherwise serialize verbatim and
/// leak the value into any serde-Serialize path that touches the
/// settings struct (bug reports, `--diagnostics`, panic envelopes, the
/// effective-config dump). Mirror the
/// [`redact_secret_opt`] contract — preserve the *shape* of the field
/// (`None` stays `None`, empty map stays empty, populated keys stay
/// visible so operators can tell which headers are set) but mask every
/// *value* as `"<redacted>"`. Header names are not secrets on their
/// own; the actual credential always lives in the value half.
fn redact_secret_map_opt<S>(
    value: &Option<BTreeMap<String, String>>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    match value {
        Some(map) => {
            let mut entries = serializer.serialize_map(Some(map.len()))?;
            for key in map.keys() {
                entries.serialize_entry(key, "<redacted>")?;
            }
            entries.end()
        }
        None => serializer.serialize_none(),
    }
}

/// Serialize a `BTreeMap` header table with every value masked to
/// `"<redacted>"` while keeping the keys visible. Header *values* on resolved
/// provider configs can carry credentials (`Authorization: Bearer …`,
/// `Apim-Subscription-Key`, `cf-aig-authorization`), so any serde-Serialize of
/// the config (diagnostics, panic envelopes, effective-config dumps) must not
/// leak them. Mirrors [`redact_secret_map_opt`] for non-`Option` fields.
fn redact_secret_map<S>(
    value: &BTreeMap<String, String>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeMap;
    let mut entries = serializer.serialize_map(Some(value.len()))?;
    for key in value.keys() {
        entries.serialize_entry(key, "<redacted>")?;
    }
    entries.end()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ModelSettings {
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Cheap model id used for low-stakes background calls (compaction
    /// summary, AI reviewer classifier, auto-approver). When `None` the
    /// per-provider built-in (`small_fast_model_for_provider`) applies.
    pub small_fast_model: Option<String>,
    pub profile: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub seed: Option<u64>,
    pub stop: Option<Vec<String>>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub stream_idle_timeout_ms: Option<u64>,
    pub store_responses: Option<bool>,
    pub selection_version: Option<u32>,
    /// Forwarded to the provider as `tool_choice` whenever tools are
    /// advertised on the request. `None` omits the field (provider
    /// default — typically `auto`). Set to `"required"` for tool-shy
    /// models routed through chat-completions aggregators (Qwen via
    /// OpenRouter, smaller MoEs) that otherwise emit a chatty preamble
    /// and finish with `stop` without calling any tool. Other accepted
    /// values: `"auto"`, `"none"`.
    pub tool_choice: Option<String>,
    /// Forwarded to the provider as `parallel_tool_calls` on the main
    /// agent (and subagent) request whenever tools are advertised.
    /// `None` (the default) omits the field, leaving the provider's own
    /// default in place — which is *parallel* for OpenAI Responses /
    /// Chat-Completions, so the common path already lets the model batch
    /// independent tool calls. Set to `true` to forward an explicit
    /// opt-in (useful on aggregator routes whose default is unknown), or
    /// `false` to force serial tool calls. Configured via
    /// `[model].parallel_tool_calls` in TOML or
    /// `SQUEEZY_PARALLEL_TOOL_CALLS`.
    pub parallel_tool_calls: Option<bool>,
    /// When `true`, append a short system-prompt nudge encouraging the
    /// model to batch independent read-only lookups (read_file / grep /
    /// definition_search) into a single assistant turn so the growing
    /// prompt prefix is re-sent on fewer round-trips. `None`/`false` (the
    /// default) leaves the prompt byte-for-byte unchanged. Configured via
    /// `[model].batch_tool_calls_hint` in TOML or
    /// `SQUEEZY_BATCH_TOOL_CALLS_HINT`.
    pub batch_tool_calls_hint: Option<bool>,
}

impl ModelSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "provider",
                "model",
                "small_fast_model",
                "profile",
                "reasoning_effort",
                "max_output_tokens",
                "temperature",
                "top_p",
                "seed",
                "stop",
                "frequency_penalty",
                "presence_penalty",
                "stream_idle_timeout_ms",
                "store_responses",
                "selection_version",
                "tool_choice",
                "parallel_tool_calls",
                "batch_tool_calls_hint",
            ],
            source,
            path,
        )?;
        let profile = string_value(table, "profile", source, &field(path, "profile"))?;
        if let Some(profile) = &profile
            && ModelProfile::parse(profile).is_none()
        {
            return Err(SqueezyError::Config(format!(
                "{source}: {} invalid profile {profile:?}; expected cheap, balanced, or strong",
                field(path, "profile")
            )));
        }
        let reasoning_effort = reasoning_effort_value(
            table,
            "reasoning_effort",
            source,
            &field(path, "reasoning_effort"),
        )?;
        Ok(Self {
            provider: string_value(table, "provider", source, &field(path, "provider"))?,
            model: string_value(table, "model", source, &field(path, "model"))?,
            small_fast_model: string_value(
                table,
                "small_fast_model",
                source,
                &field(path, "small_fast_model"),
            )?,
            profile,
            reasoning_effort,
            max_output_tokens: u32_value(
                table,
                "max_output_tokens",
                source,
                &field(path, "max_output_tokens"),
            )?,
            temperature: f32_range_value(
                table,
                "temperature",
                source,
                &field(path, "temperature"),
                0.0,
                2.0,
            )?,
            top_p: f32_range_value(table, "top_p", source, &field(path, "top_p"), 0.0, 1.0)?,
            seed: u64_nonnegative_value(table, "seed", source, &field(path, "seed"))?,
            stop: string_array_value(table, "stop", source, &field(path, "stop"))?,
            frequency_penalty: f32_range_value(
                table,
                "frequency_penalty",
                source,
                &field(path, "frequency_penalty"),
                -2.0,
                2.0,
            )?,
            presence_penalty: f32_range_value(
                table,
                "presence_penalty",
                source,
                &field(path, "presence_penalty"),
                -2.0,
                2.0,
            )?,
            stream_idle_timeout_ms: u64_value(
                table,
                "stream_idle_timeout_ms",
                source,
                &field(path, "stream_idle_timeout_ms"),
            )?,
            store_responses: bool_value(
                table,
                "store_responses",
                source,
                &field(path, "store_responses"),
            )?,
            selection_version: u32_value(
                table,
                "selection_version",
                source,
                &field(path, "selection_version"),
            )?,
            tool_choice: tool_choice_value(
                table,
                "tool_choice",
                source,
                &field(path, "tool_choice"),
            )?,
            parallel_tool_calls: bool_value(
                table,
                "parallel_tool_calls",
                source,
                &field(path, "parallel_tool_calls"),
            )?,
            batch_tool_calls_hint: bool_value(
                table,
                "batch_tool_calls_hint",
                source,
                &field(path, "batch_tool_calls_hint"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.provider, next.provider);
        replace_if_some(&mut self.model, next.model);
        replace_if_some(&mut self.small_fast_model, next.small_fast_model);
        replace_if_some(&mut self.profile, next.profile);
        replace_if_some(&mut self.reasoning_effort, next.reasoning_effort);
        replace_if_some(&mut self.max_output_tokens, next.max_output_tokens);
        replace_if_some(&mut self.temperature, next.temperature);
        replace_if_some(&mut self.top_p, next.top_p);
        replace_if_some(&mut self.seed, next.seed);
        replace_if_some(&mut self.stop, next.stop);
        replace_if_some(&mut self.frequency_penalty, next.frequency_penalty);
        replace_if_some(&mut self.presence_penalty, next.presence_penalty);
        replace_if_some(
            &mut self.stream_idle_timeout_ms,
            next.stream_idle_timeout_ms,
        );
        replace_if_some(&mut self.store_responses, next.store_responses);
        replace_if_some(&mut self.selection_version, next.selection_version);
        replace_if_some(&mut self.tool_choice, next.tool_choice);
        replace_if_some(&mut self.parallel_tool_calls, next.parallel_tool_calls);
        replace_if_some(&mut self.batch_tool_calls_hint, next.batch_tool_calls_hint);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentSettings {
    pub exploration_graph: Option<bool>,
}

impl AgentSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        if table.contains_key("exploration_compiler") {
            return Err(SqueezyError::Config(format!(
                "{source}: {}: renamed to exploration_graph",
                field(path, "exploration_compiler")
            )));
        }
        reject_unknown_keys(table, &["exploration_graph"], source, path)?;
        Ok(Self {
            exploration_graph: bool_value(
                table,
                "exploration_graph",
                source,
                &field(path, "exploration_graph"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.exploration_graph, next.exploration_graph);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BudgetSettings {
    pub max_parallel_tools: Option<usize>,
    pub tool_spill_threshold_bytes: Option<usize>,
    pub tool_preview_bytes: Option<usize>,
    pub max_tool_result_bytes_per_round: Option<usize>,
    pub tool_output_retention_days: Option<u64>,
    pub max_tool_calls_per_turn: Option<u64>,
    pub max_tool_bytes_read_per_turn: Option<u64>,
    pub max_search_files_per_turn: Option<u64>,
    pub max_session_cost_usd_micros: Option<u64>,
    pub cost_warn_percent: Option<u8>,
    pub max_round_input_tokens: Option<u64>,
}

impl BudgetSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "max_parallel_tools",
                "tool_spill_threshold_bytes",
                "tool_preview_bytes",
                "max_tool_result_bytes_per_round",
                "tool_output_retention_days",
                "max_tool_calls_per_turn",
                "max_tool_bytes_read_per_turn",
                "max_search_files_per_turn",
                "max_session_cost_usd_micros",
                "cost_warn_percent",
                "max_round_input_tokens",
            ],
            source,
            path,
        )?;
        Ok(Self {
            max_parallel_tools: usize_value(
                table,
                "max_parallel_tools",
                source,
                &field(path, "max_parallel_tools"),
            )?,
            tool_spill_threshold_bytes: usize_value(
                table,
                "tool_spill_threshold_bytes",
                source,
                &field(path, "tool_spill_threshold_bytes"),
            )?,
            tool_preview_bytes: usize_value(
                table,
                "tool_preview_bytes",
                source,
                &field(path, "tool_preview_bytes"),
            )?,
            max_tool_result_bytes_per_round: usize_value(
                table,
                "max_tool_result_bytes_per_round",
                source,
                &field(path, "max_tool_result_bytes_per_round"),
            )?,
            tool_output_retention_days: u64_value(
                table,
                "tool_output_retention_days",
                source,
                &field(path, "tool_output_retention_days"),
            )?,
            max_tool_calls_per_turn: u64_value(
                table,
                "max_tool_calls_per_turn",
                source,
                &field(path, "max_tool_calls_per_turn"),
            )?,
            max_tool_bytes_read_per_turn: u64_value(
                table,
                "max_tool_bytes_read_per_turn",
                source,
                &field(path, "max_tool_bytes_read_per_turn"),
            )?,
            max_search_files_per_turn: u64_value(
                table,
                "max_search_files_per_turn",
                source,
                &field(path, "max_search_files_per_turn"),
            )?,
            max_session_cost_usd_micros: u64_value(
                table,
                "max_session_cost_usd_micros",
                source,
                &field(path, "max_session_cost_usd_micros"),
            )?,
            cost_warn_percent: percent_value(
                table,
                "cost_warn_percent",
                source,
                &field(path, "cost_warn_percent"),
            )?,
            max_round_input_tokens: u64_value(
                table,
                "max_round_input_tokens",
                source,
                &field(path, "max_round_input_tokens"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.max_parallel_tools, next.max_parallel_tools);
        replace_if_some(
            &mut self.tool_spill_threshold_bytes,
            next.tool_spill_threshold_bytes,
        );
        replace_if_some(&mut self.tool_preview_bytes, next.tool_preview_bytes);
        replace_if_some(
            &mut self.max_tool_result_bytes_per_round,
            next.max_tool_result_bytes_per_round,
        );
        replace_if_some(
            &mut self.tool_output_retention_days,
            next.tool_output_retention_days,
        );
        replace_if_some(
            &mut self.max_tool_calls_per_turn,
            next.max_tool_calls_per_turn,
        );
        replace_if_some(
            &mut self.max_tool_bytes_read_per_turn,
            next.max_tool_bytes_read_per_turn,
        );
        replace_if_some(
            &mut self.max_search_files_per_turn,
            next.max_search_files_per_turn,
        );
        replace_if_some(
            &mut self.max_session_cost_usd_micros,
            next.max_session_cost_usd_micros,
        );
        replace_if_some(&mut self.cost_warn_percent, next.cost_warn_percent);
        replace_if_some(
            &mut self.max_round_input_tokens,
            next.max_round_input_tokens,
        );
    }
}

/// Per-turn model routing config. Resolved from `[routing]` in TOML and
/// the matching `SQUEEZY_ROUTING_*` env vars; the agent crate's
/// `turn_router` module reads these knobs to decide whether to dispatch
/// the current turn on the cheap tier and when to hand back to the
/// parent model after a false positive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Master switch for turn routing (config `[routing].enabled`, or
    /// `/router on|off`). Global; gates both the heuristic and the judge.
    pub enabled: bool,
    /// Static deterministic verb-heuristic fast-path toggle (independent of the
    /// judge). Global; not per-provider.
    pub heuristic: bool,
    pub llm_judge: bool,
    /// Char budget below which a turn inherits the previous turn's routing
    /// decision (short follow-up) instead of calling the judge. Global.
    pub follow_up_max_chars: u32,
    /// Custom judge instructions. Resolved per active provider
    /// (per-provider override → global `[routing].judge_prompt`); `None` uses
    /// the built-in per-provider prompt.
    pub judge_prompt: Option<String>,
    /// Global reroute filter: a single case-insensitive regex selecting which
    /// parent models to reroute (a leading `!` excludes; combine with `|`).
    /// Resolved per active provider (per-provider override → this global → the
    /// built-in per-provider default). Empty = fall through to the default.
    pub expensive_models: String,
    /// Hard ceiling on tool calls a cheap-routed turn may issue before
    /// the escalation detector hands back to the parent model. `0`
    /// (default) means "derive at runtime as `max_tool_calls_per_turn /
    /// 4`". Resolves via `RoutingConfig::resolved_cheap_escalation_tool_calls`.
    pub cheap_escalation_tool_calls: u64,
    pub cheap_escalation_error_threshold: u8,
    pub escalation_sticky_turns: u8,
    pub bypass_for_images: bool,
    pub large_attachment_bypass_bytes: u32,
    pub heuristic_max_chars: u32,
    pub judge_max_chars: u32,
    pub judge_model: Option<String>,
    /// User-extended heuristic verb whitelist. The built-in whitelist
    /// is deliberately narrow because false positives bypass the LLM
    /// judge — adding an entry here widens the heuristic surface but
    /// the matched prompt still has to clear the same ambiguity-marker,
    /// compound-connector, word-count, and sentence-count guards as a
    /// built-in match. Empty by default. Configured via
    /// `[routing].extra_heuristic_verbs = ["deploy", "tail"]` in TOML
    /// or `SQUEEZY_ROUTING_EXTRA_HEURISTIC_VERBS=deploy,tail` env.
    pub extra_heuristic_verbs: Vec<String>,
    /// When `true` (the default), prompts containing Linux
    /// sandbox/container/kernel keywords (`unshare`, `landlock`,
    /// `seccomp`, `sudo`, `docker`, `podman`, `/proc`, `/sys`,
    /// package-manager commands, and related terms) are routed to the
    /// parent model rather than the cheap tier. `/cheap` overrides.
    /// Disable with `[routing].linux_sandbox_sensitive_parent = false`
    /// or `SQUEEZY_ROUTING_LINUX_SANDBOX_SENSITIVE_PARENT=false`.
    pub linux_sandbox_sensitive_parent: bool,
}

impl RoutingConfig {
    fn from_settings_and_env(
        settings: RoutingSettings,
        get_var: &mut impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self {
            enabled: get_var("SQUEEZY_ROUTING_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.enabled.unwrap_or(DEFAULT_ROUTING_ENABLED)),
            heuristic: get_var("SQUEEZY_ROUTING_HEURISTIC")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.heuristic.unwrap_or(DEFAULT_ROUTING_HEURISTIC)),
            follow_up_max_chars: get_var("SQUEEZY_ROUTING_FOLLOW_UP_MAX_CHARS")
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .or(settings.follow_up_max_chars)
                .unwrap_or(DEFAULT_ROUTING_FOLLOW_UP_MAX_CHARS),
            judge_prompt: get_var("SQUEEZY_ROUTING_JUDGE_PROMPT").or(settings.judge_prompt),
            expensive_models: get_var("SQUEEZY_ROUTING_EXPENSIVE_MODELS")
                .or(settings.expensive_models)
                .unwrap_or_default(),
            llm_judge: get_var("SQUEEZY_ROUTING_LLM_JUDGE")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.llm_judge.unwrap_or(DEFAULT_ROUTING_LLM_JUDGE)),
            cheap_escalation_tool_calls: parse_u64(
                get_var("SQUEEZY_ROUTING_CHEAP_ESCALATION_TOOL_CALLS"),
                settings.cheap_escalation_tool_calls.unwrap_or(0),
            ),
            cheap_escalation_error_threshold: get_var(
                "SQUEEZY_ROUTING_CHEAP_ESCALATION_ERROR_THRESHOLD",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u8>().ok())
            .or(settings.cheap_escalation_error_threshold)
            .unwrap_or(DEFAULT_ROUTING_CHEAP_ESCALATION_ERROR_THRESHOLD),
            escalation_sticky_turns: get_var("SQUEEZY_ROUTING_ESCALATION_STICKY_TURNS")
                .as_deref()
                .and_then(|raw| raw.parse::<u8>().ok())
                .or(settings.escalation_sticky_turns)
                .unwrap_or(DEFAULT_ROUTING_ESCALATION_STICKY_TURNS),
            bypass_for_images: get_var("SQUEEZY_ROUTING_BYPASS_FOR_IMAGES")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(
                    settings
                        .bypass_for_images
                        .unwrap_or(DEFAULT_ROUTING_BYPASS_FOR_IMAGES),
                ),
            large_attachment_bypass_bytes: get_var("SQUEEZY_ROUTING_LARGE_ATTACHMENT_BYPASS_BYTES")
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .or(settings.large_attachment_bypass_bytes)
                .unwrap_or(DEFAULT_ROUTING_LARGE_ATTACHMENT_BYPASS_BYTES),
            heuristic_max_chars: get_var("SQUEEZY_ROUTING_HEURISTIC_MAX_CHARS")
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .or(settings.heuristic_max_chars)
                .unwrap_or(DEFAULT_ROUTING_HEURISTIC_MAX_CHARS),
            judge_max_chars: get_var("SQUEEZY_ROUTING_JUDGE_MAX_CHARS")
                .as_deref()
                .and_then(|raw| raw.parse::<u32>().ok())
                .or(settings.judge_max_chars)
                .unwrap_or(DEFAULT_ROUTING_JUDGE_MAX_CHARS),
            judge_model: get_var("SQUEEZY_ROUTING_JUDGE_MODEL").or(settings.judge_model),
            extra_heuristic_verbs: get_var("SQUEEZY_ROUTING_EXTRA_HEURISTIC_VERBS")
                .map(|raw| {
                    raw.split(',')
                        .map(|verb| verb.trim().to_string())
                        .filter(|verb| !verb.is_empty())
                        .collect()
                })
                .or(settings.extra_heuristic_verbs)
                .unwrap_or_default(),
            linux_sandbox_sensitive_parent: get_var(
                "SQUEEZY_ROUTING_LINUX_SANDBOX_SENSITIVE_PARENT",
            )
            .as_deref()
            .map(parse_enabled_bool)
            .unwrap_or(
                settings
                    .linux_sandbox_sensitive_parent
                    .unwrap_or(DEFAULT_ROUTING_LINUX_SANDBOX_SENSITIVE_PARENT),
            ),
        }
    }

    /// Resolves the in-turn escalation ceiling for tool calls. When the
    /// user leaves `cheap_escalation_tool_calls` at the `0` sentinel we
    /// derive a value from the parent's `max_tool_calls_per_turn` rather
    /// than hard-coding a number, so the threshold scales with the
    /// user's existing budget choices.
    pub fn resolved_cheap_escalation_tool_calls(&self, max_tool_calls_per_turn: u64) -> u64 {
        if self.cheap_escalation_tool_calls > 0 {
            self.cheap_escalation_tool_calls
        } else {
            (max_tool_calls_per_turn / 4).max(1)
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RoutingSettings {
    pub enabled: Option<bool>,
    pub heuristic: Option<bool>,
    pub llm_judge: Option<bool>,
    pub follow_up_max_chars: Option<u32>,
    pub judge_prompt: Option<String>,
    pub expensive_models: Option<String>,
    pub cheap_escalation_tool_calls: Option<u64>,
    pub cheap_escalation_error_threshold: Option<u8>,
    pub escalation_sticky_turns: Option<u8>,
    pub bypass_for_images: Option<bool>,
    pub large_attachment_bypass_bytes: Option<u32>,
    pub heuristic_max_chars: Option<u32>,
    pub judge_max_chars: Option<u32>,
    pub judge_model: Option<String>,
    pub extra_heuristic_verbs: Option<Vec<String>>,
    pub linux_sandbox_sensitive_parent: Option<bool>,
}

impl RoutingSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "heuristic",
                "llm_judge",
                "follow_up_max_chars",
                "judge_prompt",
                "expensive_models",
                "cheap_escalation_tool_calls",
                "cheap_escalation_error_threshold",
                "escalation_sticky_turns",
                "bypass_for_images",
                "large_attachment_bypass_bytes",
                "heuristic_max_chars",
                "judge_max_chars",
                "judge_model",
                "extra_heuristic_verbs",
                "linux_sandbox_sensitive_parent",
            ],
            source,
            path,
        )?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            heuristic: bool_value(table, "heuristic", source, &field(path, "heuristic"))?,
            llm_judge: bool_value(table, "llm_judge", source, &field(path, "llm_judge"))?,
            follow_up_max_chars: u32_value(
                table,
                "follow_up_max_chars",
                source,
                &field(path, "follow_up_max_chars"),
            )?,
            judge_prompt: string_value(
                table,
                "judge_prompt",
                source,
                &field(path, "judge_prompt"),
            )?,
            expensive_models: string_value(
                table,
                "expensive_models",
                source,
                &field(path, "expensive_models"),
            )?,
            cheap_escalation_tool_calls: u64_value(
                table,
                "cheap_escalation_tool_calls",
                source,
                &field(path, "cheap_escalation_tool_calls"),
            )?,
            cheap_escalation_error_threshold: u8_value(
                table,
                "cheap_escalation_error_threshold",
                source,
                &field(path, "cheap_escalation_error_threshold"),
            )?,
            escalation_sticky_turns: u8_value(
                table,
                "escalation_sticky_turns",
                source,
                &field(path, "escalation_sticky_turns"),
            )?,
            bypass_for_images: bool_value(
                table,
                "bypass_for_images",
                source,
                &field(path, "bypass_for_images"),
            )?,
            large_attachment_bypass_bytes: u32_value(
                table,
                "large_attachment_bypass_bytes",
                source,
                &field(path, "large_attachment_bypass_bytes"),
            )?,
            heuristic_max_chars: u32_value(
                table,
                "heuristic_max_chars",
                source,
                &field(path, "heuristic_max_chars"),
            )?,
            judge_max_chars: u32_value(
                table,
                "judge_max_chars",
                source,
                &field(path, "judge_max_chars"),
            )?,
            judge_model: string_value(table, "judge_model", source, &field(path, "judge_model"))?,
            extra_heuristic_verbs: string_array_value(
                table,
                "extra_heuristic_verbs",
                source,
                &field(path, "extra_heuristic_verbs"),
            )?,
            linux_sandbox_sensitive_parent: bool_value(
                table,
                "linux_sandbox_sensitive_parent",
                source,
                &field(path, "linux_sandbox_sensitive_parent"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.heuristic, next.heuristic);
        replace_if_some(&mut self.llm_judge, next.llm_judge);
        replace_if_some(&mut self.follow_up_max_chars, next.follow_up_max_chars);
        replace_if_some(&mut self.judge_prompt, next.judge_prompt);
        replace_if_some(&mut self.expensive_models, next.expensive_models);
        replace_if_some(
            &mut self.cheap_escalation_tool_calls,
            next.cheap_escalation_tool_calls,
        );
        replace_if_some(
            &mut self.cheap_escalation_error_threshold,
            next.cheap_escalation_error_threshold,
        );
        replace_if_some(
            &mut self.escalation_sticky_turns,
            next.escalation_sticky_turns,
        );
        replace_if_some(&mut self.bypass_for_images, next.bypass_for_images);
        replace_if_some(
            &mut self.large_attachment_bypass_bytes,
            next.large_attachment_bypass_bytes,
        );
        replace_if_some(&mut self.heuristic_max_chars, next.heuristic_max_chars);
        replace_if_some(&mut self.judge_max_chars, next.judge_max_chars);
        replace_if_some(&mut self.judge_model, next.judge_model);
        merge_string_lists(&mut self.extra_heuristic_verbs, next.extra_heuristic_verbs);
        replace_if_some(
            &mut self.linux_sandbox_sensitive_parent,
            next.linux_sandbox_sensitive_parent,
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchemaConfig {
    pub lazy_schema_loading: bool,
    pub core: Vec<String>,
    pub discoverable: Vec<String>,
    /// Names that must be filtered out before tools are advertised to
    /// the model, even if they would otherwise be in `core` or
    /// `discoverable`. Used by graph-vs-no-graph eval scenarios to
    /// hide the semantic-graph family (`repo_map`, `decl_search`, …)
    /// so the model is forced to fall back to lexical tools.
    pub excluded: Vec<String>,
}

impl Default for ToolSchemaConfig {
    fn default() -> Self {
        Self {
            lazy_schema_loading: true,
            core: DEFAULT_CORE_TOOL_NAMES
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
            discoverable: Vec::new(),
            excluded: Vec::new(),
        }
    }
}

impl ToolSchemaConfig {
    pub fn from_settings(settings: ToolSchemaSettings) -> Result<Self> {
        let defaults = Self::default();
        if let (Some(core), Some(discoverable)) = (&settings.core, &settings.discoverable) {
            reject_tool_schema_overlap(core, discoverable)?;
        }
        let mut core = defaults.core;
        if let Some(additional_core) = settings.core {
            for tool in additional_core {
                if !core.contains(&tool) {
                    core.push(tool);
                }
            }
        }
        let discoverable = settings.discoverable.unwrap_or(defaults.discoverable);
        core.retain(|tool| !discoverable.contains(tool));
        let excluded = settings.excluded.unwrap_or(defaults.excluded);
        Ok(Self {
            lazy_schema_loading: settings
                .lazy_schema_loading
                .unwrap_or(defaults.lazy_schema_loading),
            core,
            discoverable,
            excluded,
        })
    }

    pub fn core_contains(&self, name: &str) -> bool {
        self.core.iter().any(|tool| tool == name)
    }

    pub fn discoverable_contains(&self, name: &str) -> bool {
        self.discoverable.iter().any(|tool| tool == name)
    }

    pub fn is_excluded(&self, name: &str) -> bool {
        self.excluded.iter().any(|tool| tool == name)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ToolSchemaSettings {
    pub checkpoints_enabled: Option<bool>,
    pub lazy_schema_loading: Option<bool>,
    pub core: Option<Vec<String>>,
    pub discoverable: Option<Vec<String>>,
    pub excluded: Option<Vec<String>>,
}

impl ToolSchemaSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "checkpoints_enabled",
                "lazy_schema_loading",
                "core",
                "discoverable",
                "excluded",
            ],
            source,
            path,
        )?;
        Ok(Self {
            checkpoints_enabled: bool_value(
                table,
                "checkpoints_enabled",
                source,
                &field(path, "checkpoints_enabled"),
            )?,
            lazy_schema_loading: bool_value(
                table,
                "lazy_schema_loading",
                source,
                &field(path, "lazy_schema_loading"),
            )?,
            core: string_array_value(table, "core", source, &field(path, "core"))?,
            discoverable: string_array_value(
                table,
                "discoverable",
                source,
                &field(path, "discoverable"),
            )?,
            excluded: string_array_value(table, "excluded", source, &field(path, "excluded"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.checkpoints_enabled, next.checkpoints_enabled);
        replace_if_some(&mut self.lazy_schema_loading, next.lazy_schema_loading);
        merge_string_lists(&mut self.core, next.core);
        merge_string_lists(&mut self.discoverable, next.discoverable);
        merge_string_lists(&mut self.excluded, next.excluded);
    }
}

fn reject_tool_schema_overlap(core: &[String], discoverable: &[String]) -> Result<()> {
    let core = core.iter().collect::<BTreeSet<_>>();
    let overlap = discoverable
        .iter()
        .filter(|name| core.contains(name))
        .cloned()
        .collect::<Vec<_>>();
    if overlap.is_empty() {
        return Ok(());
    }
    Err(SqueezyError::Config(format!(
        "[tools] core and discoverable overlap: {}",
        overlap.join(", ")
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionMode {
    Allow,
    Ask,
    Deny,
}

impl PermissionMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "allow" | "allowed" => Some(Self::Allow),
            "ask" | "prompt" | "confirm" => Some(Self::Ask),
            "deny" | "denied" | "refuse" => Some(Self::Deny),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

pub type PermissionAction = PermissionMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionPolicyMode {
    Default,
    AutoReview,
    FullAccess,
    Custom,
}

impl PermissionPolicyMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" | "workspace" | "workspace_write" | "workspace-write" => Some(Self::Default),
            "auto_review" | "auto-review" | "autoreview" | "auto" => Some(Self::AutoReview),
            "full_access" | "full-access" | "danger_full_access" | "danger-full-access" => {
                Some(Self::FullAccess)
            }
            "custom" | "granular" => Some(Self::Custom),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AutoReview => "auto_review",
            Self::FullAccess => "full_access",
            Self::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Plan,
    #[default]
    Build,
}

impl SessionMode {
    /// Parse the two canonical session-mode names. The accepted values are
    /// only `plan` and `build` (case-insensitive, surrounding whitespace
    /// ignored) so that the user-visible vocabulary stays in sync with
    /// `as_str`, error messages, and config docs. Anything else returns
    /// `None` so configuration loaders can surface a precise error.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "plan" => Some(Self::Plan),
            "build" => Some(Self::Build),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
        }
    }

    /// Compact wire form for lock-free storage in an `AtomicU8`. `from_u8`
    /// rejects unknown discriminants and the caller decides on a safe
    /// default; see `Agent::session_mode` for the in-process use.
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Plan => 0,
            Self::Build => 1,
        }
    }

    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Plan),
            1 => Some(Self::Build),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionResumePicker {
    #[default]
    Ask,
    Never,
}

impl SessionResumePicker {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ask" | "always" | "on" | "true" => Some(Self::Ask),
            "never" | "off" | "false" => Some(Self::Never),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Never => "never",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionSettings {
    pub mode: Option<SessionMode>,
    pub resume_picker: Option<SessionResumePicker>,
    pub log_dir: Option<PathBuf>,
    pub log_retention_days: Option<u64>,
    pub log_retention_archive_days: Option<u64>,
    pub max_event_bytes: Option<usize>,
    pub max_session_bytes: Option<usize>,
}

impl SessionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "mode",
                "resume_picker",
                "log_dir",
                "log_retention_days",
                "log_retention_archive_days",
                "max_event_bytes",
                "max_session_bytes",
            ],
            source,
            path,
        )?;
        let mode = match table.get("mode") {
            Some(value) => {
                let value = value
                    .as_str()
                    .ok_or_else(|| type_error(source, &field(path, "mode"), "string"))?;
                Some(parse_session_mode_value(
                    value,
                    source,
                    &field(path, "mode"),
                )?)
            }
            None => None,
        };
        Ok(Self {
            mode,
            resume_picker: session_resume_picker_value(
                table,
                "resume_picker",
                source,
                &field(path, "resume_picker"),
            )?,
            log_dir: path_value(table, "log_dir", source, &field(path, "log_dir"))?,
            log_retention_days: u64_value(
                table,
                "log_retention_days",
                source,
                &field(path, "log_retention_days"),
            )?,
            log_retention_archive_days: u64_value(
                table,
                "log_retention_archive_days",
                source,
                &field(path, "log_retention_archive_days"),
            )?,
            max_event_bytes: usize_value(
                table,
                "max_event_bytes",
                source,
                &field(path, "max_event_bytes"),
            )?,
            max_session_bytes: usize_value(
                table,
                "max_session_bytes",
                source,
                &field(path, "max_session_bytes"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.mode, next.mode);
        replace_if_some(&mut self.resume_picker, next.resume_picker);
        replace_if_some(&mut self.log_dir, next.log_dir);
        replace_if_some(&mut self.log_retention_days, next.log_retention_days);
        replace_if_some(
            &mut self.log_retention_archive_days,
            next.log_retention_archive_days,
        );
        replace_if_some(&mut self.max_event_bytes, next.max_event_bytes);
        replace_if_some(&mut self.max_session_bytes, next.max_session_bytes);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogConfig {
    pub log_dir: Option<PathBuf>,
    pub log_retention_days: u64,
    /// Days an archived session lingers before the retention sweep
    /// permanently deletes it. Setting this to `0` disables the archive
    /// sweep; archived sessions are then retained until removed by hand.
    #[serde(default = "default_log_retention_archive_days")]
    pub log_retention_archive_days: u64,
    pub max_event_bytes: usize,
    pub max_session_bytes: usize,
}

fn default_log_retention_archive_days() -> u64 {
    DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS
}

impl SessionLogConfig {
    fn from_settings(settings: &SessionSettings) -> Self {
        Self {
            log_dir: settings.log_dir.clone(),
            log_retention_days: settings
                .log_retention_days
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SESSION_LOG_RETENTION_DAYS),
            // `0` is a valid explicit value here: it disables the archive
            // sweep and lets archived sessions accumulate until the user
            // removes them by hand. Anything else falls back to the
            // built-in default rather than silently being treated as zero.
            log_retention_archive_days: settings
                .log_retention_archive_days
                .unwrap_or(DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS),
            max_event_bytes: settings
                .max_event_bytes
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SESSION_MAX_EVENT_BYTES),
            max_session_bytes: settings
                .max_session_bytes
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_SESSION_MAX_SESSION_BYTES),
        }
    }
}

impl Default for SessionLogConfig {
    fn default() -> Self {
        Self {
            log_dir: None,
            log_retention_days: DEFAULT_SESSION_LOG_RETENTION_DAYS,
            log_retention_archive_days: DEFAULT_SESSION_LOG_RETENTION_ARCHIVE_DAYS,
            max_event_bytes: DEFAULT_SESSION_MAX_EVENT_BYTES,
            max_session_bytes: DEFAULT_SESSION_MAX_SESSION_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionSettings {
    pub compaction_enabled: Option<bool>,
    pub compaction_estimated_tokens: Option<u64>,
    pub compaction_min_items: Option<usize>,
    pub compaction_recent_items: Option<usize>,
    pub compaction_max_summary_bytes: Option<usize>,
    pub repo_doc_max_bytes: Option<usize>,
    pub user_memory_max_bytes: Option<usize>,
    pub enabled_mid_turn: Option<bool>,
    pub model_context_window: Option<u64>,
    pub effective_context_window_percent: Option<u8>,
    pub baseline_reserve_tokens: Option<u64>,
    pub fallback_window_tokens: Option<u64>,
    pub max_context_tokens: Option<u64>,
    pub trim_at_percent: Option<u8>,
    pub warn_at_percent: Option<u8>,
    /// Deprecated; parsed for back-compat with older configs.
    pub threshold_percent: Option<u8>,
    pub strategy: Option<CompactionStrategy>,
    pub model_assisted_model: Option<String>,
    pub model_assisted_max_output_tokens: Option<u32>,
    pub model_assisted_timeout_secs: Option<u64>,
    pub layered_fallback_extractive_threshold_tokens: Option<u32>,
    pub micro_compaction_enabled: Option<bool>,
    pub micro_compaction_threshold_percent: Option<u8>,
    pub micro_compaction_keep_recent: Option<usize>,
}

impl ContextCompactionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "compaction_enabled",
                "compaction_estimated_tokens",
                "compaction_min_items",
                "compaction_recent_items",
                "compaction_max_summary_bytes",
                "repo_doc_max_bytes",
                "user_memory_max_bytes",
                "enabled_mid_turn",
                "model_context_window",
                "effective_context_window_percent",
                "baseline_reserve_tokens",
                "fallback_window_tokens",
                "max_context_tokens",
                "trim_at_percent",
                "warn_at_percent",
                "threshold_percent",
                "strategy",
                "model_assisted_model",
                "model_assisted_max_output_tokens",
                "model_assisted_timeout_secs",
                "layered_fallback_extractive_threshold_tokens",
                "micro_compaction_enabled",
                "micro_compaction_threshold_percent",
                "micro_compaction_keep_recent",
            ],
            source,
            path,
        )?;
        Ok(Self {
            compaction_enabled: bool_value(
                table,
                "compaction_enabled",
                source,
                &field(path, "compaction_enabled"),
            )?,
            compaction_estimated_tokens: u64_value(
                table,
                "compaction_estimated_tokens",
                source,
                &field(path, "compaction_estimated_tokens"),
            )?,
            compaction_min_items: usize_value(
                table,
                "compaction_min_items",
                source,
                &field(path, "compaction_min_items"),
            )?,
            compaction_recent_items: usize_value(
                table,
                "compaction_recent_items",
                source,
                &field(path, "compaction_recent_items"),
            )?,
            compaction_max_summary_bytes: usize_value(
                table,
                "compaction_max_summary_bytes",
                source,
                &field(path, "compaction_max_summary_bytes"),
            )?,
            repo_doc_max_bytes: usize_value(
                table,
                "repo_doc_max_bytes",
                source,
                &field(path, "repo_doc_max_bytes"),
            )?,
            user_memory_max_bytes: usize_value(
                table,
                "user_memory_max_bytes",
                source,
                &field(path, "user_memory_max_bytes"),
            )?,
            enabled_mid_turn: bool_value(
                table,
                "enabled_mid_turn",
                source,
                &field(path, "enabled_mid_turn"),
            )?,
            model_context_window: u64_value(
                table,
                "model_context_window",
                source,
                &field(path, "model_context_window"),
            )?,
            effective_context_window_percent: u8_value(
                table,
                "effective_context_window_percent",
                source,
                &field(path, "effective_context_window_percent"),
            )?,
            baseline_reserve_tokens: u64_value(
                table,
                "baseline_reserve_tokens",
                source,
                &field(path, "baseline_reserve_tokens"),
            )?,
            fallback_window_tokens: u64_value(
                table,
                "fallback_window_tokens",
                source,
                &field(path, "fallback_window_tokens"),
            )?,
            max_context_tokens: u64_value(
                table,
                "max_context_tokens",
                source,
                &field(path, "max_context_tokens"),
            )?,
            trim_at_percent: u8_value(
                table,
                "trim_at_percent",
                source,
                &field(path, "trim_at_percent"),
            )?,
            warn_at_percent: u8_value(
                table,
                "warn_at_percent",
                source,
                &field(path, "warn_at_percent"),
            )?,
            threshold_percent: u8_value(
                table,
                "threshold_percent",
                source,
                &field(path, "threshold_percent"),
            )?,
            strategy: {
                let raw = string_value(table, "strategy", source, &field(path, "strategy"))?;
                match raw {
                    None => None,
                    Some(value) => Some(CompactionStrategy::parse(&value).ok_or_else(|| {
                        SqueezyError::Config(format!(
                            "{source}: {}: expected one of extractive | model_assisted | layered_fallback",
                            field(path, "strategy")
                        ))
                    })?),
                }
            },
            model_assisted_model: string_value(
                table,
                "model_assisted_model",
                source,
                &field(path, "model_assisted_model"),
            )?,
            model_assisted_max_output_tokens: u32_value(
                table,
                "model_assisted_max_output_tokens",
                source,
                &field(path, "model_assisted_max_output_tokens"),
            )?,
            model_assisted_timeout_secs: u64_value(
                table,
                "model_assisted_timeout_secs",
                source,
                &field(path, "model_assisted_timeout_secs"),
            )?,
            layered_fallback_extractive_threshold_tokens: u32_value(
                table,
                "layered_fallback_extractive_threshold_tokens",
                source,
                &field(path, "layered_fallback_extractive_threshold_tokens"),
            )?,
            micro_compaction_enabled: bool_value(
                table,
                "micro_compaction_enabled",
                source,
                &field(path, "micro_compaction_enabled"),
            )?,
            micro_compaction_threshold_percent: u8_value(
                table,
                "micro_compaction_threshold_percent",
                source,
                &field(path, "micro_compaction_threshold_percent"),
            )?,
            micro_compaction_keep_recent: usize_value(
                table,
                "micro_compaction_keep_recent",
                source,
                &field(path, "micro_compaction_keep_recent"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.compaction_enabled, next.compaction_enabled);
        replace_if_some(
            &mut self.compaction_estimated_tokens,
            next.compaction_estimated_tokens,
        );
        replace_if_some(&mut self.compaction_min_items, next.compaction_min_items);
        replace_if_some(
            &mut self.compaction_recent_items,
            next.compaction_recent_items,
        );
        replace_if_some(
            &mut self.compaction_max_summary_bytes,
            next.compaction_max_summary_bytes,
        );
        replace_if_some(&mut self.repo_doc_max_bytes, next.repo_doc_max_bytes);
        replace_if_some(&mut self.user_memory_max_bytes, next.user_memory_max_bytes);
        replace_if_some(&mut self.enabled_mid_turn, next.enabled_mid_turn);
        replace_if_some(&mut self.model_context_window, next.model_context_window);
        replace_if_some(
            &mut self.effective_context_window_percent,
            next.effective_context_window_percent,
        );
        replace_if_some(
            &mut self.baseline_reserve_tokens,
            next.baseline_reserve_tokens,
        );
        replace_if_some(
            &mut self.fallback_window_tokens,
            next.fallback_window_tokens,
        );
        replace_if_some(&mut self.max_context_tokens, next.max_context_tokens);
        replace_if_some(&mut self.trim_at_percent, next.trim_at_percent);
        replace_if_some(&mut self.warn_at_percent, next.warn_at_percent);
        replace_if_some(&mut self.threshold_percent, next.threshold_percent);
        replace_if_some(&mut self.strategy, next.strategy);
        replace_if_some(&mut self.model_assisted_model, next.model_assisted_model);
        replace_if_some(
            &mut self.model_assisted_max_output_tokens,
            next.model_assisted_max_output_tokens,
        );
        replace_if_some(
            &mut self.model_assisted_timeout_secs,
            next.model_assisted_timeout_secs,
        );
        replace_if_some(
            &mut self.layered_fallback_extractive_threshold_tokens,
            next.layered_fallback_extractive_threshold_tokens,
        );
        replace_if_some(
            &mut self.micro_compaction_enabled,
            next.micro_compaction_enabled,
        );
        replace_if_some(
            &mut self.micro_compaction_threshold_percent,
            next.micro_compaction_threshold_percent,
        );
        replace_if_some(
            &mut self.micro_compaction_keep_recent,
            next.micro_compaction_keep_recent,
        );
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentSettings {
    pub enabled: Option<bool>,
    pub explore_enabled: Option<bool>,
    pub explore_model: Option<String>,
    pub max_concurrent: Option<usize>,
    pub max_tool_calls_per_call: Option<u64>,
    pub max_tool_bytes_read_per_call: Option<u64>,
    pub max_search_files_per_call: Option<u64>,
    pub max_model_rounds: Option<usize>,
    pub max_summary_tokens: Option<u32>,
    pub max_runtime_secs: Option<u64>,
    pub include_transcript: Option<bool>,
}

impl SubagentSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "explore_enabled",
                "explore_model",
                "max_concurrent",
                "max_tool_calls_per_call",
                "max_tool_bytes_read_per_call",
                "max_search_files_per_call",
                "max_model_rounds",
                "max_summary_tokens",
                "max_runtime_secs",
                "include_transcript",
            ],
            source,
            path,
        )?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            explore_enabled: bool_value(
                table,
                "explore_enabled",
                source,
                &field(path, "explore_enabled"),
            )?,
            explore_model: string_value(
                table,
                "explore_model",
                source,
                &field(path, "explore_model"),
            )?,
            max_concurrent: usize_value(
                table,
                "max_concurrent",
                source,
                &field(path, "max_concurrent"),
            )?,
            max_tool_calls_per_call: u64_value(
                table,
                "max_tool_calls_per_call",
                source,
                &field(path, "max_tool_calls_per_call"),
            )?,
            max_tool_bytes_read_per_call: u64_value(
                table,
                "max_tool_bytes_read_per_call",
                source,
                &field(path, "max_tool_bytes_read_per_call"),
            )?,
            max_search_files_per_call: u64_value(
                table,
                "max_search_files_per_call",
                source,
                &field(path, "max_search_files_per_call"),
            )?,
            max_model_rounds: usize_value(
                table,
                "max_model_rounds",
                source,
                &field(path, "max_model_rounds"),
            )?,
            max_summary_tokens: u32_value(
                table,
                "max_summary_tokens",
                source,
                &field(path, "max_summary_tokens"),
            )?,
            max_runtime_secs: u64_nonnegative_value(
                table,
                "max_runtime_secs",
                source,
                &field(path, "max_runtime_secs"),
            )?,
            include_transcript: bool_value(
                table,
                "include_transcript",
                source,
                &field(path, "include_transcript"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.explore_enabled, next.explore_enabled);
        replace_if_some(&mut self.explore_model, next.explore_model);
        replace_if_some(&mut self.max_concurrent, next.max_concurrent);
        replace_if_some(
            &mut self.max_tool_calls_per_call,
            next.max_tool_calls_per_call,
        );
        replace_if_some(
            &mut self.max_tool_bytes_read_per_call,
            next.max_tool_bytes_read_per_call,
        );
        replace_if_some(
            &mut self.max_search_files_per_call,
            next.max_search_files_per_call,
        );
        replace_if_some(&mut self.max_model_rounds, next.max_model_rounds);
        replace_if_some(&mut self.max_summary_tokens, next.max_summary_tokens);
        replace_if_some(&mut self.max_runtime_secs, next.max_runtime_secs);
        replace_if_some(&mut self.include_transcript, next.include_transcript);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentConfig {
    pub enabled: bool,
    pub explore_enabled: bool,
    pub explore_model: Option<String>,
    pub max_concurrent: usize,
    pub max_tool_calls_per_call: u64,
    pub max_tool_bytes_read_per_call: u64,
    pub max_search_files_per_call: u64,
    pub max_model_rounds: usize,
    pub max_summary_tokens: u32,
    /// Wall-clock cap on a single subagent run. `None` (set via TOML
    /// `max_runtime_secs = 0` or `SQUEEZY_SUBAGENT_MAX_RUNTIME_SECS=0`)
    /// disables the timeout entirely; cancellation and round caps remain.
    pub max_runtime_secs: Option<u64>,
    /// When `true`, the structured subagent result returned to the parent
    /// carries a `transcript` field with the child's assistant + tool
    /// trace. Default `false` — the parent sees only the final fields
    /// (`summary`, `supporting_receipts`, `files_touched`), keeping the
    /// parent loop's context tight.
    pub include_transcript: bool,
}

impl SubagentConfig {
    fn from_settings_and_env(
        settings: SubagentSettings,
        get_var: &mut impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self {
            enabled: get_var("SQUEEZY_SUBAGENTS_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.enabled.unwrap_or(true)),
            explore_enabled: get_var("SQUEEZY_EXPLORE_SUBAGENT_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.explore_enabled.unwrap_or(true)),
            explore_model: get_var("SQUEEZY_EXPLORE_MODEL")
                .or(settings.explore_model)
                .filter(|value| !value.trim().is_empty()),
            max_concurrent: {
                let raw = parse_usize(
                    get_var("SQUEEZY_SUBAGENT_MAX_CONCURRENT"),
                    settings
                        .max_concurrent
                        .unwrap_or(DEFAULT_SUBAGENT_MAX_CONCURRENT),
                );
                raw.max(1)
            },
            max_tool_calls_per_call: parse_u64(
                get_var("SQUEEZY_SUBAGENT_MAX_TOOL_CALLS_PER_CALL"),
                settings
                    .max_tool_calls_per_call
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL),
            ),
            max_tool_bytes_read_per_call: parse_u64(
                get_var("SQUEEZY_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL"),
                settings
                    .max_tool_bytes_read_per_call
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL),
            ),
            max_search_files_per_call: parse_u64(
                get_var("SQUEEZY_SUBAGENT_MAX_SEARCH_FILES_PER_CALL"),
                settings
                    .max_search_files_per_call
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL),
            ),
            max_model_rounds: parse_usize(
                get_var("SQUEEZY_SUBAGENT_MAX_MODEL_ROUNDS"),
                settings
                    .max_model_rounds
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS),
            ),
            max_summary_tokens: get_var("SQUEEZY_SUBAGENT_MAX_SUMMARY_TOKENS")
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|value| *value > 0)
                .or(settings.max_summary_tokens)
                .unwrap_or(DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS),
            max_runtime_secs: {
                let raw = get_var("SQUEEZY_SUBAGENT_MAX_RUNTIME_SECS")
                    .and_then(|value| value.parse::<u64>().ok())
                    .or(settings.max_runtime_secs)
                    .unwrap_or(DEFAULT_SUBAGENT_MAX_RUNTIME_SECS);
                if raw == 0 { None } else { Some(raw) }
            },
            include_transcript: get_var("SQUEEZY_SUBAGENT_INCLUDE_TRANSCRIPT")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.include_transcript.unwrap_or(false)),
        }
    }
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            explore_enabled: true,
            explore_model: None,
            max_concurrent: DEFAULT_SUBAGENT_MAX_CONCURRENT,
            max_tool_calls_per_call: DEFAULT_SUBAGENT_MAX_TOOL_CALLS_PER_CALL,
            max_tool_bytes_read_per_call: DEFAULT_SUBAGENT_MAX_TOOL_BYTES_READ_PER_CALL,
            max_search_files_per_call: DEFAULT_SUBAGENT_MAX_SEARCH_FILES_PER_CALL,
            max_model_rounds: DEFAULT_SUBAGENT_MAX_MODEL_ROUNDS,
            max_summary_tokens: DEFAULT_SUBAGENT_MAX_SUMMARY_TOKENS,
            max_runtime_secs: None,
            include_transcript: false,
        }
    }
}

/// How the compaction summary is produced. Default is `Extractive`, which
/// preserves the historical deterministic / no-model-call behavior. The two
/// other variants opt into model-assisted summarization with strict
/// extractive fallback on error / timeout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionStrategy {
    #[default]
    Extractive,
    ModelAssisted,
    LayeredFallback,
}

impl CompactionStrategy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "extractive" => Some(Self::Extractive),
            "model_assisted" | "model-assisted" => Some(Self::ModelAssisted),
            "layered_fallback" | "layered-fallback" => Some(Self::LayeredFallback),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Extractive => "extractive",
            Self::ModelAssisted => "model_assisted",
            Self::LayeredFallback => "layered_fallback",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionConfig {
    pub enabled: bool,
    /// Context window assumed when the active model's real window is unknown.
    /// Every percent threshold resolves against the real window when known,
    /// else this value.
    pub fallback_window_tokens: u64,
    /// Optional hard cap (in tokens) on the summarize threshold, independent of
    /// the window. `None` lets thresholds scale with the window; set it to keep
    /// requests small on very large windows (an opt-in economy cap).
    pub max_context_tokens: Option<u64>,
    pub min_items: usize,
    pub recent_items: usize,
    pub max_summary_bytes: usize,
    /// Maximum bytes of concatenated AGENTS.md content stitched into the
    /// base instructions at session start. 0 disables ingestion.
    pub repo_doc_max_bytes: usize,
    /// Maximum bytes of `~/.squeezy/MEMORY.md` (or lowercase `memory.md`)
    /// stitched into the base instructions at session start. 0 disables
    /// ingestion. The static file is the only cross-session memory
    /// surface; see `docs/internal/MEMORY_SCOPE.md` for the deferred
    /// tool-mediated pipeline decision.
    pub user_memory_max_bytes: usize,
    /// When true, the turn loop runs the trim (micro) pass between LLM events
    /// so a long tool-heavy turn reclaims older tool-output bytes before it can
    /// outgrow the window. Summarize never runs mid-turn; it waits for the turn
    /// boundary or the forced overflow path.
    pub enabled_mid_turn: bool,
    /// Token budget for the active model. When `None`, thresholds resolve
    /// against `fallback_window_tokens`. Normally auto-derived from the model
    /// registry; an explicit value here overrides the registry.
    pub model_context_window: Option<u64>,
    /// Optional override for the percent of the raw window treated as usable
    /// (the rest is headroom). `None` lets the limit resolver use the curated
    /// model's percent, falling back to 95. Surfaces the previously-hidden
    /// effective-window reduction so it is inspectable and tunable.
    pub effective_context_window_percent: Option<u8>,
    /// Optional override for the flat token reserve carved off the effective
    /// window for system framing. `None` uses
    /// `squeezy_llm::DEFAULT_BASELINE_RESERVE_TOKENS` (12_000).
    pub baseline_reserve_tokens: Option<u64>,
    /// Fraction of the effective window (0..=100) at which the pre-summarize
    /// nudge fires. Sits below the summarize point (the effective window).
    /// Capped to 100 on read.
    pub warn_at_percent: u8,
    /// Deprecated; retained for back-compat config parsing but no longer drives
    /// compaction (summarize now fires at the effective window).
    pub threshold_percent: u8,
    /// Summary generation strategy. Default `Extractive` preserves current
    /// behavior; other variants opt-in to model-assisted summarization with
    /// extractive fallback.
    pub strategy: CompactionStrategy,
    /// Cheap model id used for model-assisted compaction. Required when
    /// `strategy != Extractive`; the path falls back to extractive if unset
    /// or if the provider rejects the model.
    pub model_assisted_model: Option<String>,
    pub model_assisted_max_output_tokens: u32,
    pub model_assisted_timeout_secs: u64,
    pub layered_fallback_extractive_threshold_tokens: u32,
    /// Master switch for the mid-tier "micro" compaction pass. When true
    /// and `enabled_mid_turn` is also true, the agent attempts to clear
    /// older `FunctionCallOutput` payloads in place before falling
    /// through to full compaction. Disabled callers go straight from
    /// no-op to full compaction.
    pub micro_compaction_enabled: bool,
    /// Fraction of the effective window (0..=100) at which the trim (micro) pass
    /// fires. Sits well below the summarize point (the effective window) so
    /// trimming reclaims tool-output bytes long before the lossy summarize tier.
    /// Capped to 100.
    pub trim_at_percent: u8,
    /// Keep this many newest compactable tool results verbatim; older
    /// results are rewritten to a structured placeholder.
    pub micro_compaction_keep_recent: usize,
}

impl ContextCompactionConfig {
    fn from_settings_and_env(
        settings: ContextCompactionSettings,
        get_var: &mut impl FnMut(&str) -> Option<String>,
    ) -> Self {
        Self {
            enabled: get_var("SQUEEZY_CONTEXT_COMPACTION_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.compaction_enabled.unwrap_or(true)),
            fallback_window_tokens: parse_u64(
                get_var("SQUEEZY_CONTEXT_FALLBACK_WINDOW_TOKENS")
                    .or_else(|| get_var("SQUEEZY_CONTEXT_COMPACTION_ESTIMATED_TOKENS")),
                settings
                    .fallback_window_tokens
                    .or(settings.compaction_estimated_tokens)
                    .unwrap_or(DEFAULT_CONTEXT_FALLBACK_WINDOW_TOKENS),
            ),
            max_context_tokens: get_var("SQUEEZY_CONTEXT_MAX_CONTEXT_TOKENS")
                .as_deref()
                .and_then(|raw| raw.parse::<u64>().ok())
                .or(settings.max_context_tokens),
            min_items: parse_usize(
                get_var("SQUEEZY_CONTEXT_COMPACTION_MIN_ITEMS"),
                settings
                    .compaction_min_items
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS),
            ),
            recent_items: parse_usize(
                get_var("SQUEEZY_CONTEXT_COMPACTION_RECENT_ITEMS"),
                settings
                    .compaction_recent_items
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS),
            ),
            max_summary_bytes: parse_usize(
                get_var("SQUEEZY_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES"),
                settings
                    .compaction_max_summary_bytes
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES),
            ),
            repo_doc_max_bytes: parse_usize(
                get_var("SQUEEZY_CONTEXT_REPO_DOC_MAX_BYTES"),
                settings
                    .repo_doc_max_bytes
                    .unwrap_or(DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES),
            ),
            user_memory_max_bytes: parse_usize(
                get_var("SQUEEZY_CONTEXT_USER_MEMORY_MAX_BYTES"),
                settings
                    .user_memory_max_bytes
                    .unwrap_or(DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES),
            ),
            enabled_mid_turn: get_var("SQUEEZY_CONTEXT_COMPACTION_ENABLED_MID_TURN")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.enabled_mid_turn.unwrap_or(true)),
            model_context_window: get_var("SQUEEZY_CONTEXT_MODEL_CONTEXT_WINDOW")
                .as_deref()
                .and_then(|raw| raw.parse::<u64>().ok())
                .or(settings.model_context_window),
            effective_context_window_percent: get_var(
                "SQUEEZY_CONTEXT_EFFECTIVE_CONTEXT_WINDOW_PERCENT",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u8>().ok())
            .or(settings.effective_context_window_percent)
            // Clamp BOTH env and file values to 1..=100 — a checked-in 200 would
            // otherwise double the usable window and 0 would zero it. The config
            // screen enforces the same range; this guards raw TOML/env.
            .map(|percent| percent.clamp(1, 100)),
            baseline_reserve_tokens: get_var("SQUEEZY_CONTEXT_BASELINE_RESERVE_TOKENS")
                .as_deref()
                .and_then(|raw| raw.parse::<u64>().ok())
                .or(settings.baseline_reserve_tokens),
            threshold_percent: clamp_percent(
                get_var("SQUEEZY_CONTEXT_COMPACTION_THRESHOLD_PERCENT")
                    .as_deref()
                    .and_then(|raw| raw.parse::<u8>().ok())
                    .or(settings.threshold_percent)
                    .unwrap_or(DEFAULT_CONTEXT_COMPACTION_THRESHOLD_PERCENT),
            ),
            warn_at_percent: clamp_percent(
                get_var("SQUEEZY_CONTEXT_WARN_AT_PERCENT")
                    .as_deref()
                    .and_then(|raw| raw.parse::<u8>().ok())
                    .or(settings.warn_at_percent)
                    .unwrap_or(DEFAULT_CONTEXT_WARN_AT_PERCENT),
            ),
            strategy: get_var("SQUEEZY_CONTEXT_COMPACTION_STRATEGY")
                .as_deref()
                .and_then(CompactionStrategy::parse)
                .or(settings.strategy)
                .unwrap_or_default(),
            model_assisted_model: get_var("SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_MODEL")
                .or_else(|| settings.model_assisted_model.clone()),
            model_assisted_max_output_tokens: get_var(
                "SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u32>().ok())
            .or(settings.model_assisted_max_output_tokens)
            .unwrap_or(DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS),
            model_assisted_timeout_secs: get_var(
                "SQUEEZY_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u64>().ok())
            .or(settings.model_assisted_timeout_secs)
            .unwrap_or(DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS),
            layered_fallback_extractive_threshold_tokens: get_var(
                "SQUEEZY_CONTEXT_COMPACTION_LAYERED_FALLBACK_THRESHOLD_TOKENS",
            )
            .as_deref()
            .and_then(|raw| raw.parse::<u32>().ok())
            .or(settings.layered_fallback_extractive_threshold_tokens)
            .unwrap_or(DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS),
            micro_compaction_enabled: get_var("SQUEEZY_CONTEXT_MICRO_COMPACTION_ENABLED")
                .as_deref()
                .map(parse_enabled_bool)
                .unwrap_or(settings.micro_compaction_enabled.unwrap_or(true)),
            trim_at_percent: clamp_percent(
                get_var("SQUEEZY_CONTEXT_TRIM_AT_PERCENT")
                    .or_else(|| get_var("SQUEEZY_CONTEXT_MICRO_COMPACTION_THRESHOLD_PERCENT"))
                    .as_deref()
                    .and_then(|raw| raw.parse::<u8>().ok())
                    .or(settings.trim_at_percent)
                    .or(settings.micro_compaction_threshold_percent)
                    .unwrap_or(DEFAULT_CONTEXT_TRIM_AT_PERCENT),
            ),
            micro_compaction_keep_recent: parse_usize(
                get_var("SQUEEZY_CONTEXT_MICRO_COMPACTION_KEEP_RECENT"),
                settings
                    .micro_compaction_keep_recent
                    .unwrap_or(DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT),
            ),
        }
    }

    /// The model's real context window when known, else `fallback_window_tokens`.
    /// This is the raw window; the usable budget thresholds resolve against is
    /// [`effective_window`](Self::effective_window).
    pub fn resolve_window(&self) -> u64 {
        match self.model_context_window {
            Some(window) if window > 0 => window,
            _ => self.fallback_window_tokens,
        }
    }

    /// The usable context budget every threshold resolves against: the raw
    /// window reduced to `effective_context_window_percent` of itself, minus the
    /// `baseline_reserve_tokens` carved off for system framing, and finally
    /// bounded by the opt-in `max_context_tokens` economy cap. This mirrors the
    /// limit resolver's `effective_window_tokens` so compaction folds at the
    /// same usable budget the request sizer targets. The percent/reserve
    /// overrides default to the resolver's 95% / 12K when unset (the
    /// curated-per-model values live in the resolver, not here).
    pub fn effective_window(&self) -> u64 {
        let raw = self.resolve_window();
        let percent = self
            .effective_context_window_percent
            .unwrap_or(DEFAULT_CONTEXT_EFFECTIVE_WINDOW_PERCENT)
            .clamp(1, 100) as u64;
        let reserve = self
            .baseline_reserve_tokens
            .unwrap_or(DEFAULT_CONTEXT_BASELINE_RESERVE_TOKENS);
        let usable = raw
            .saturating_mul(percent)
            .saturating_div(100)
            .saturating_sub(reserve);
        let capped = match self.max_context_tokens {
            Some(cap) if cap > 0 => usable.min(cap),
            _ => usable,
        };
        capped.max(1)
    }

    /// `percent` of the effective window.
    fn window_fraction(&self, percent: u8) -> u64 {
        self.effective_window()
            .saturating_mul(percent.min(100) as u64)
            .saturating_div(100)
    }

    /// Token usage at which the trim (micro) pass fires. Trimming is cheap and
    /// structure-preserving, so this is intentionally low.
    pub fn trim_threshold(&self) -> u64 {
        self.window_fraction(self.trim_at_percent)
    }

    /// Token usage at which the pre-summarize nudge fires.
    pub fn warn_threshold(&self) -> u64 {
        self.window_fraction(self.warn_at_percent)
    }

    /// Token usage at which the lossy summarize tier fires: the full effective
    /// (usable) window. Trim and warn are fractions of the same effective
    /// window, so `trim < warn < summarize` holds for every window size and
    /// economy cap (the `baseline_reserve_tokens` reduction already in
    /// `effective_window` is what keeps room for the next turn's reply).
    pub fn summarize_threshold(&self) -> u64 {
        self.effective_window()
    }

    /// High-water mark above which the post-turn summarize gate bypasses the
    /// `min_items` floor so a "few but enormous" conversation still summarizes
    /// proactively.
    pub fn min_items_bypass_threshold(&self) -> u64 {
        self.effective_window()
            .saturating_mul(HIGH_WATER_BYPASS_PCT)
            .saturating_div(100)
    }
}

fn clamp_percent(value: u8) -> u8 {
    value.min(100)
}

impl Default for ContextCompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            fallback_window_tokens: DEFAULT_CONTEXT_FALLBACK_WINDOW_TOKENS,
            max_context_tokens: None,
            min_items: DEFAULT_CONTEXT_COMPACTION_MIN_ITEMS,
            recent_items: DEFAULT_CONTEXT_COMPACTION_RECENT_ITEMS,
            max_summary_bytes: DEFAULT_CONTEXT_COMPACTION_MAX_SUMMARY_BYTES,
            repo_doc_max_bytes: DEFAULT_CONTEXT_REPO_DOC_MAX_BYTES,
            user_memory_max_bytes: DEFAULT_CONTEXT_USER_MEMORY_MAX_BYTES,
            enabled_mid_turn: true,
            model_context_window: None,
            effective_context_window_percent: None,
            baseline_reserve_tokens: None,
            warn_at_percent: DEFAULT_CONTEXT_WARN_AT_PERCENT,
            threshold_percent: DEFAULT_CONTEXT_COMPACTION_THRESHOLD_PERCENT,
            strategy: CompactionStrategy::default(),
            model_assisted_model: None,
            model_assisted_max_output_tokens:
                DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_MAX_OUTPUT_TOKENS,
            model_assisted_timeout_secs: DEFAULT_CONTEXT_COMPACTION_MODEL_ASSISTED_TIMEOUT_SECS,
            layered_fallback_extractive_threshold_tokens:
                DEFAULT_CONTEXT_COMPACTION_LAYERED_FALLBACK_EXTRACTIVE_THRESHOLD_TOKENS,
            micro_compaction_enabled: true,
            trim_at_percent: DEFAULT_CONTEXT_TRIM_AT_PERCENT,
            micro_compaction_keep_recent: DEFAULT_CONTEXT_MICRO_COMPACTION_KEEP_RECENT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionCapability {
    Read,
    Search,
    Edit,
    Shell,
    Network,
    Mcp,
    Git,
    Compiler,
    Destructive,
}

impl PermissionCapability {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read" => Some(Self::Read),
            "search" => Some(Self::Search),
            "edit" | "write" => Some(Self::Edit),
            "shell" | "bash" | "command" => Some(Self::Shell),
            "network" | "web" => Some(Self::Network),
            "mcp" => Some(Self::Mcp),
            "git" => Some(Self::Git),
            "compiler" | "verify" => Some(Self::Compiler),
            "destructive" | "dangerous" => Some(Self::Destructive),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Search => "search",
            Self::Edit => "edit",
            Self::Shell => "shell",
            Self::Network => "network",
            Self::Mcp => "mcp",
            Self::Git => "git",
            Self::Compiler => "compiler",
            Self::Destructive => "destructive",
        }
    }
}

/// Severity is ordered `Low < Medium < High < Critical` (the variant
/// declaration order), so callers can compare against a ceiling — e.g. the AI
/// reviewer refuses to auto-allow anything `>= High`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PermissionRisk {
    Low,
    Medium,
    High,
    Critical,
}

impl PermissionRisk {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionRuleSource {
    Builtin,
    User,
    Project,
    Session,
}

impl PermissionRuleSource {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "builtin" => Some(Self::Builtin),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "session" => Some(Self::Session),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
            Self::Session => "session",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub capability: String,
    pub target: String,
    pub action: PermissionAction,
    pub source: PermissionRuleSource,
    pub reason: Option<String>,
    /// When `true` and `action == Deny`, the matching verdict suppresses the
    /// per-call narrative on the tool-result string sent back to the model
    /// (the audit JSONL still records the full reason). Only meaningful for
    /// `Deny` rules; loaders reject `silent = true` on `Allow`/`Ask` to keep
    /// the field's semantics narrow. The use case is boilerplate policy: an
    /// absolute deny rule like `rm -rf /` or writes to `.git/config` does not
    /// need to spell out `capability=...; target=...; risk=...` to the model
    /// on every retry — the model only needs to know the call is rejected and
    /// move on. Auditability stays in the JSONL log.
    #[serde(default, skip_serializing_if = "is_false")]
    pub silent: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl PermissionRule {
    pub fn new(
        capability: impl Into<String>,
        target: impl Into<String>,
        action: PermissionAction,
        source: PermissionRuleSource,
        reason: Option<String>,
    ) -> Self {
        Self {
            capability: capability.into(),
            target: target.into(),
            action,
            source,
            reason,
            silent: false,
        }
    }

    /// Mark this rule as silent. Only meaningful for `Deny` rules; the loader
    /// rejects `silent = true` on `Allow`/`Ask`, but this setter does not
    /// re-check because in-memory builders may compose pieces in either order.
    /// Callers responsible for upholding the invariant are the TOML loader and
    /// any future builders that synthesize rules from typed sources.
    pub fn with_silent(mut self, silent: bool) -> Self {
        self.silent = silent;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub call_id: String,
    pub tool_name: String,
    pub capability: PermissionCapability,
    pub target: String,
    pub risk: PermissionRisk,
    pub summary: String,
    pub metadata: BTreeMap<String, String>,
    pub suggested_rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionVerdict {
    pub action: PermissionAction,
    pub matched_rule: Option<PermissionRule>,
    pub reason: String,
    /// True when this verdict came from a `silent` deny rule. The agent uses
    /// this to send a minimal `"action denied by policy"` to the model in place
    /// of the structured `reason`, while the audit JSONL still receives the
    /// full reason via `log_permission_verdict`. Only set when `action` is
    /// `Deny`; other actions ignore the flag.
    pub silent: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PermissionSettings {
    pub mode: Option<PermissionPolicyMode>,
    pub read: Option<PermissionMode>,
    pub search: Option<PermissionMode>,
    pub edit: Option<PermissionMode>,
    pub shell: Option<PermissionMode>,
    pub ignored_search: Option<PermissionMode>,
    pub web: Option<PermissionMode>,
    pub mcp: Option<PermissionMode>,
    pub git: Option<PermissionMode>,
    pub compiler: Option<PermissionMode>,
    pub destructive: Option<PermissionMode>,
    pub shell_classifier: Option<bool>,
    pub custom: Option<PermissionCustomSettings>,
    pub ai_reviewer: Option<AiReviewerSettings>,
    pub shell_sandbox: Option<ShellSandboxSettings>,
    pub rules: Vec<PermissionRule>,
}

impl PermissionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "mode",
                "read",
                "search",
                "edit",
                "shell",
                "ignored_search",
                "web",
                "mcp",
                "git",
                "compiler",
                "destructive",
                "shell_classifier",
                "custom",
                "ai_reviewer",
                "shell_sandbox",
                "rules",
            ],
            source,
            path,
        )?;
        let mode = string_value(table, "mode", source, &field(path, "mode"))?
            .map(|mode| {
                PermissionPolicyMode::parse(&mode).ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "{source}: {path}.mode invalid value {mode:?}; expected default, auto_review, full_access, or custom"
                    ))
                })
            })
            .transpose()?;
        Ok(Self {
            mode,
            read: permission_value(table, "read", source, &field(path, "read"))?,
            search: permission_value(table, "search", source, &field(path, "search"))?,
            edit: permission_value(table, "edit", source, &field(path, "edit"))?,
            shell: permission_value(table, "shell", source, &field(path, "shell"))?,
            ignored_search: permission_value(
                table,
                "ignored_search",
                source,
                &field(path, "ignored_search"),
            )?,
            web: permission_value(table, "web", source, &field(path, "web"))?,
            mcp: permission_value(table, "mcp", source, &field(path, "mcp"))?,
            git: permission_value(table, "git", source, &field(path, "git"))?,
            compiler: permission_value(table, "compiler", source, &field(path, "compiler"))?,
            destructive: permission_value(
                table,
                "destructive",
                source,
                &field(path, "destructive"),
            )?,
            shell_classifier: bool_value(
                table,
                "shell_classifier",
                source,
                &field(path, "shell_classifier"),
            )?,
            custom: optional_table(table, "custom", source)?
                .map(|table| {
                    PermissionCustomSettings::from_table(table, source, &field(path, "custom"))
                })
                .transpose()?,
            ai_reviewer: optional_table(table, "ai_reviewer", source)?
                .map(|table| {
                    AiReviewerSettings::from_table(table, source, &field(path, "ai_reviewer"))
                })
                .transpose()?,
            shell_sandbox: optional_table(table, "shell_sandbox", source)?
                .map(|table| {
                    ShellSandboxSettings::from_table(table, source, &field(path, "shell_sandbox"))
                })
                .transpose()?,
            rules: permission_rules_value(table, source, &field(path, "rules"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.mode, next.mode);
        replace_if_some(&mut self.read, next.read);
        replace_if_some(&mut self.search, next.search);
        replace_if_some(&mut self.edit, next.edit);
        replace_if_some(&mut self.shell, next.shell);
        replace_if_some(&mut self.ignored_search, next.ignored_search);
        replace_if_some(&mut self.web, next.web);
        replace_if_some(&mut self.mcp, next.mcp);
        replace_if_some(&mut self.git, next.git);
        replace_if_some(&mut self.compiler, next.compiler);
        replace_if_some(&mut self.destructive, next.destructive);
        replace_if_some(&mut self.shell_classifier, next.shell_classifier);
        merge_option(
            &mut self.custom,
            next.custom,
            PermissionCustomSettings::merge,
        );
        merge_option(
            &mut self.ai_reviewer,
            next.ai_reviewer,
            AiReviewerSettings::merge,
        );
        merge_option(
            &mut self.shell_sandbox,
            next.shell_sandbox,
            ShellSandboxSettings::merge,
        );
        self.rules.extend(next.rules);
    }

    fn has_legacy_defaults(&self) -> bool {
        self.read.is_some()
            || self.search.is_some()
            || self.edit.is_some()
            || self.shell.is_some()
            || self.ignored_search.is_some()
            || self.web.is_some()
            || self.mcp.is_some()
            || self.git.is_some()
            || self.compiler.is_some()
            || self.destructive.is_some()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PermissionCustomSettings {
    pub read: Option<PermissionMode>,
    pub search: Option<PermissionMode>,
    pub edit: Option<PermissionMode>,
    pub shell: Option<PermissionMode>,
    pub ignored_search: Option<PermissionMode>,
    pub network: Option<PermissionMode>,
    pub mcp: Option<PermissionMode>,
    pub git: Option<PermissionMode>,
    pub compiler: Option<PermissionMode>,
    pub destructive: Option<PermissionMode>,
}

impl PermissionCustomSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "read",
                "search",
                "edit",
                "shell",
                "ignored_search",
                "network",
                "mcp",
                "git",
                "compiler",
                "destructive",
            ],
            source,
            path,
        )?;
        Ok(Self {
            read: permission_value(table, "read", source, &field(path, "read"))?,
            search: permission_value(table, "search", source, &field(path, "search"))?,
            edit: permission_value(table, "edit", source, &field(path, "edit"))?,
            shell: permission_value(table, "shell", source, &field(path, "shell"))?,
            ignored_search: permission_value(
                table,
                "ignored_search",
                source,
                &field(path, "ignored_search"),
            )?,
            network: permission_value(table, "network", source, &field(path, "network"))?,
            mcp: permission_value(table, "mcp", source, &field(path, "mcp"))?,
            git: permission_value(table, "git", source, &field(path, "git"))?,
            compiler: permission_value(table, "compiler", source, &field(path, "compiler"))?,
            destructive: permission_value(
                table,
                "destructive",
                source,
                &field(path, "destructive"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.read, next.read);
        replace_if_some(&mut self.search, next.search);
        replace_if_some(&mut self.edit, next.edit);
        replace_if_some(&mut self.shell, next.shell);
        replace_if_some(&mut self.ignored_search, next.ignored_search);
        replace_if_some(&mut self.network, next.network);
        replace_if_some(&mut self.mcp, next.mcp);
        replace_if_some(&mut self.git, next.git);
        replace_if_some(&mut self.compiler, next.compiler);
        replace_if_some(&mut self.destructive, next.destructive);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AiReviewerSettings {
    pub enabled: Option<bool>,
    pub model: Option<String>,
    pub allow_capabilities: Option<Vec<String>>,
    pub policy_file: Option<String>,
    pub policy: Option<String>,
    pub timeout_secs: Option<u64>,
    pub max_transcript_tokens: Option<u64>,
}

impl AiReviewerSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "model",
                "allow_capabilities",
                "policy_file",
                "policy",
                "timeout_secs",
                "max_transcript_tokens",
            ],
            source,
            path,
        )?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            model: string_value(table, "model", source, &field(path, "model"))?,
            allow_capabilities: string_array_value(
                table,
                "allow_capabilities",
                source,
                &field(path, "allow_capabilities"),
            )?,
            policy_file: string_value(table, "policy_file", source, &field(path, "policy_file"))?,
            policy: string_value(table, "policy", source, &field(path, "policy"))?,
            timeout_secs: u64_value(table, "timeout_secs", source, &field(path, "timeout_secs"))?,
            max_transcript_tokens: u64_value(
                table,
                "max_transcript_tokens",
                source,
                &field(path, "max_transcript_tokens"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.model, next.model);
        replace_if_some(&mut self.allow_capabilities, next.allow_capabilities);
        replace_if_some(&mut self.policy_file, next.policy_file);
        replace_if_some(&mut self.policy, next.policy);
        replace_if_some(&mut self.timeout_secs, next.timeout_secs);
        replace_if_some(&mut self.max_transcript_tokens, next.max_transcript_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AiReviewerConfig {
    pub enabled: bool,
    pub model: Option<String>,
    pub allow_capabilities: Vec<PermissionCapability>,
    pub policy_file: Option<PathBuf>,
    /// Extra instructions appended to the base judging policy (the built-in
    /// `APPROVAL_POLICY.md` or `policy_file`). Lets a project tighten/extend the
    /// policy without replacing it wholesale.
    pub policy: Option<String>,
    pub timeout_secs: u64,
    /// Sliding-window transcript budget for the reviewer prompt. Keeps the most
    /// recent turns whole and compacts older entries into a single summary
    /// line so late-turn permission requests retain earlier intent context.
    pub max_transcript_tokens: usize,
}

pub const DEFAULT_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS: usize = 4_000;
const MIN_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS: u64 = 512;
const MAX_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS: u64 = 32_000;

impl Default for AiReviewerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: None,
            allow_capabilities: vec![PermissionCapability::Read, PermissionCapability::Search],
            policy_file: None,
            policy: None,
            timeout_secs: 15,
            max_transcript_tokens: DEFAULT_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS,
        }
    }
}

impl AiReviewerConfig {
    fn from_settings(settings: Option<AiReviewerSettings>, source: &str) -> Result<Self> {
        let mut config = Self::default();
        let Some(settings) = settings else {
            return Ok(config);
        };
        if let Some(enabled) = settings.enabled {
            config.enabled = enabled;
        }
        if let Some(model) = settings.model {
            let model = model.trim();
            if !model.is_empty() {
                config.model = Some(model.to_string());
            }
        }
        if let Some(policy_file) = settings.policy_file {
            let policy_file = policy_file.trim();
            if !policy_file.is_empty() {
                config.policy_file = Some(expand_home_path(PathBuf::from(policy_file)));
            }
        }
        if let Some(policy) = settings.policy {
            let policy = policy.trim();
            if !policy.is_empty() {
                config.policy = Some(policy.to_string());
            }
        }
        if let Some(timeout_secs) = settings.timeout_secs {
            if !(1..=120).contains(&timeout_secs) {
                return Err(SqueezyError::Config(format!(
                    "{source}: permissions.ai_reviewer.timeout_secs {timeout_secs} outside supported range 1..=120"
                )));
            }
            config.timeout_secs = timeout_secs;
        }
        if let Some(max_transcript_tokens) = settings.max_transcript_tokens {
            if !(MIN_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS..=MAX_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS)
                .contains(&max_transcript_tokens)
            {
                return Err(SqueezyError::Config(format!(
                    "{source}: permissions.ai_reviewer.max_transcript_tokens {max_transcript_tokens} outside supported range {MIN_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS}..={MAX_AI_REVIEWER_MAX_TRANSCRIPT_TOKENS}"
                )));
            }
            config.max_transcript_tokens = max_transcript_tokens as usize;
        }
        if let Some(allow_capabilities) = settings.allow_capabilities {
            let mut parsed = Vec::new();
            for capability in allow_capabilities {
                let Some(capability) = PermissionCapability::parse(&capability) else {
                    return Err(SqueezyError::Config(format!(
                        "{source}: permissions.ai_reviewer.allow_capabilities contains invalid capability {capability:?}"
                    )));
                };
                if !parsed.contains(&capability) {
                    parsed.push(capability);
                }
            }
            config.allow_capabilities = parsed;
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ShellSandboxSettings {
    pub mode: Option<String>,
    pub network: Option<String>,
    pub audit: Option<bool>,
    pub kill_grace_ms: Option<u64>,
    pub env_allowlist: Option<Vec<String>>,
    pub read_roots: Option<Vec<String>>,
    pub write_roots: Option<Vec<String>>,
    pub protected_metadata_names: Option<Vec<String>>,
    pub sensitive_path_patterns: Option<Vec<String>>,
    /// When `true`, the user-provided `sensitive_path_patterns` REPLACE the
    /// built-in floor. The default behavior (`false` / unset) extends the
    /// floor so a config that lists a single project pattern still keeps
    /// the `.ssh/**`, `.aws/**`, `.netrc`, etc. denials.
    pub replace_sensitive_path_patterns: Option<bool>,
    /// Windows-only: `disabled`, `restricted_token` (default), or `elevated`.
    pub windows_sandbox_level: Option<String>,
}

impl ShellSandboxSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "mode",
                "network",
                "audit",
                "kill_grace_ms",
                "env_allowlist",
                "read_roots",
                "write_roots",
                "protected_metadata_names",
                "sensitive_path_patterns",
                "replace_sensitive_path_patterns",
                "windows_sandbox_level",
            ],
            source,
            path,
        )?;
        Ok(Self {
            mode: string_value(table, "mode", source, &field(path, "mode"))?,
            network: string_value(table, "network", source, &field(path, "network"))?,
            audit: bool_value(table, "audit", source, &field(path, "audit"))?,
            kill_grace_ms: u64_value(
                table,
                "kill_grace_ms",
                source,
                &field(path, "kill_grace_ms"),
            )?,
            env_allowlist: string_array_value(
                table,
                "env_allowlist",
                source,
                &field(path, "env_allowlist"),
            )?,
            read_roots: string_array_value(
                table,
                "read_roots",
                source,
                &field(path, "read_roots"),
            )?,
            write_roots: string_array_value(
                table,
                "write_roots",
                source,
                &field(path, "write_roots"),
            )?,
            protected_metadata_names: string_array_value(
                table,
                "protected_metadata_names",
                source,
                &field(path, "protected_metadata_names"),
            )?,
            sensitive_path_patterns: string_array_value(
                table,
                "sensitive_path_patterns",
                source,
                &field(path, "sensitive_path_patterns"),
            )?,
            replace_sensitive_path_patterns: bool_value(
                table,
                "replace_sensitive_path_patterns",
                source,
                &field(path, "replace_sensitive_path_patterns"),
            )?,
            windows_sandbox_level: string_value(
                table,
                "windows_sandbox_level",
                source,
                &field(path, "windows_sandbox_level"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.mode, next.mode);
        replace_if_some(&mut self.network, next.network);
        replace_if_some(&mut self.audit, next.audit);
        replace_if_some(&mut self.kill_grace_ms, next.kill_grace_ms);
        replace_if_some(&mut self.env_allowlist, next.env_allowlist);
        merge_string_lists(&mut self.read_roots, next.read_roots);
        merge_string_lists(&mut self.write_roots, next.write_roots);
        replace_if_some(
            &mut self.protected_metadata_names,
            next.protected_metadata_names,
        );
        replace_if_some(
            &mut self.sensitive_path_patterns,
            next.sensitive_path_patterns,
        );
        replace_if_some(
            &mut self.replace_sensitive_path_patterns,
            next.replace_sensitive_path_patterns,
        );
        replace_if_some(&mut self.windows_sandbox_level, next.windows_sandbox_level);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellSandboxMode {
    Required,
    BestEffort,
    Off,
    External,
}

impl ShellSandboxMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" => Some(Self::Required),
            "best_effort" | "best-effort" => Some(Self::BestEffort),
            "off" | "disabled" => Some(Self::Off),
            "external" | "external_sandbox" | "external-sandbox" => Some(Self::External),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::BestEffort => "best_effort",
            Self::Off => "off",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShellSandboxNetworkPolicy {
    DenyByDefault,
    AllowWhenApproved,
}

impl ShellSandboxNetworkPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "deny_by_default" | "deny-by-default" => Some(Self::DenyByDefault),
            "allow_when_approved" | "allow-when-approved" => Some(Self::AllowWhenApproved),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DenyByDefault => "deny_by_default",
            Self::AllowWhenApproved => "allow_when_approved",
        }
    }
}

/// Which Windows sandbox backend the shell sandbox should use. Ignored on
/// non-Windows platforms (macOS/Linux have their own backends).
///
/// * `RestrictedToken` (default) — per-spawn restricted-token filesystem
///   isolation; no admin required. Enforces filesystem *writes* and write
///   carve-outs. Reads and network are not enforced on this tier.
/// * `Elevated` — opt-in tier provisioned by `squeezy doctor --sandbox-setup`
///   (one-time UAC): runs commands as a dedicated sandbox user with full
///   read-deny and WFP network egress control. Falls back to `RestrictedToken`
///   (best-effort) or denies (required) when setup has not been completed.
/// * `Disabled` — no OS isolation; Job Object process-tree cleanup only (the
///   historical Windows behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowsSandboxLevel {
    Disabled,
    RestrictedToken,
    Elevated,
}

impl WindowsSandboxLevel {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" | "none" => Some(Self::Disabled),
            "restricted_token" | "restricted-token" | "restricted" => Some(Self::RestrictedToken),
            "elevated" => Some(Self::Elevated),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::RestrictedToken => "restricted_token",
            Self::Elevated => "elevated",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellSandboxConfig {
    pub mode: ShellSandboxMode,
    pub network: ShellSandboxNetworkPolicy,
    pub audit: bool,
    pub kill_grace_ms: u64,
    pub env_allowlist: Vec<String>,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub protected_metadata_names: Vec<String>,
    pub sensitive_path_patterns: Vec<String>,
    /// Windows-only backend selection. Defaults to `RestrictedToken` so Windows
    /// shells get filesystem isolation with no configuration. Ignored elsewhere.
    pub windows_sandbox_level: WindowsSandboxLevel,
}

impl Default for ShellSandboxConfig {
    fn default() -> Self {
        Self {
            mode: ShellSandboxMode::BestEffort,
            network: ShellSandboxNetworkPolicy::DenyByDefault,
            audit: true,
            kill_grace_ms: 250,
            env_allowlist: default_shell_env_allowlist(),
            read_roots: Vec::new(),
            write_roots: Vec::new(),
            protected_metadata_names: default_protected_metadata_names(),
            sensitive_path_patterns: default_sensitive_path_patterns(),
            windows_sandbox_level: WindowsSandboxLevel::RestrictedToken,
        }
    }
}

const SHELL_SANDBOX_KILL_GRACE_MIN_MS: u64 = 10;
const SHELL_SANDBOX_KILL_GRACE_MAX_MS: u64 = 60_000;

impl ShellSandboxConfig {
    fn from_settings(
        settings: Option<ShellSandboxSettings>,
        source: &str,
        workspace_root: &Path,
    ) -> Result<Self> {
        let mut config = Self::default();
        let Some(settings) = settings else {
            return Ok(config);
        };
        if let Some(mode) = settings.mode {
            config.mode = ShellSandboxMode::parse(&mode).ok_or_else(|| {
                SqueezyError::Config(format!(
                    "{source}: permissions.shell_sandbox.mode invalid value {mode:?}; expected required, best_effort, off, or external"
                ))
            })?;
        }
        if let Some(network) = settings.network {
            config.network = ShellSandboxNetworkPolicy::parse(&network).ok_or_else(|| {
                SqueezyError::Config(format!(
                    "{source}: permissions.shell_sandbox.network invalid value {network:?}; expected deny_by_default or allow_when_approved"
                ))
            })?;
        }
        if let Some(audit) = settings.audit {
            config.audit = audit;
        }
        if let Some(kill_grace_ms) = settings.kill_grace_ms {
            if !(SHELL_SANDBOX_KILL_GRACE_MIN_MS..=SHELL_SANDBOX_KILL_GRACE_MAX_MS)
                .contains(&kill_grace_ms)
            {
                return Err(SqueezyError::Config(format!(
                    "{source}: permissions.shell_sandbox.kill_grace_ms {kill_grace_ms} \
                     outside supported range {SHELL_SANDBOX_KILL_GRACE_MIN_MS}..={SHELL_SANDBOX_KILL_GRACE_MAX_MS}"
                )));
            }
            config.kill_grace_ms = kill_grace_ms;
        }
        if let Some(env_allowlist) = settings.env_allowlist {
            for pattern in &env_allowlist {
                validate_env_allowlist_pattern(pattern, source)?;
            }
            if env_allowlist.is_empty() {
                tracing::warn!(
                    target: "squeezy::permissions",
                    source = %source,
                    "permissions.shell_sandbox.env_allowlist was set to an empty list; \
                     shell commands will run with an empty environment"
                );
            }
            config.env_allowlist = env_allowlist;
        }
        // sensitive_path_patterns uses UNION semantics: user-provided patterns
        // EXTEND the built-in floor (.ssh/**, .aws/**, .netrc, …) rather than
        // replacing it. The built-in floor cannot be silently disabled by
        // listing a single project-specific pattern. To explicitly disable
        // the floor, set `replace_sensitive_path_patterns = true`.
        if let Some(sensitive_path_patterns) = settings.sensitive_path_patterns {
            for pattern in &sensitive_path_patterns {
                validate_sensitive_path_pattern(pattern, source)?;
            }
            if settings.replace_sensitive_path_patterns.unwrap_or(false) {
                if sensitive_path_patterns.is_empty() {
                    tracing::warn!(
                        target: "squeezy::permissions",
                        source = %source,
                        "permissions.shell_sandbox.sensitive_path_patterns was replaced with an empty list; \
                         pre-spawn shell sensitive-path checks are now disabled"
                    );
                }
                config.sensitive_path_patterns = sensitive_path_patterns;
            } else {
                let mut merged = config.sensitive_path_patterns.clone();
                for pattern in sensitive_path_patterns {
                    if !merged.contains(&pattern) {
                        merged.push(pattern);
                    }
                }
                config.sensitive_path_patterns = merged;
            }
        }
        let root_validation = (settings.read_roots.is_some() || settings.write_roots.is_some())
            .then(|| ShellSandboxRootValidation::new(workspace_root));
        if let Some(read_roots) = settings.read_roots {
            config.read_roots = validate_shell_sandbox_roots(
                read_roots,
                "read_roots",
                source,
                &config.sensitive_path_patterns,
                root_validation
                    .as_ref()
                    .expect("root validation context exists when roots are configured"),
            )?;
        }
        if let Some(write_roots) = settings.write_roots {
            config.write_roots = validate_shell_sandbox_roots(
                write_roots,
                "write_roots",
                source,
                &config.sensitive_path_patterns,
                root_validation
                    .as_ref()
                    .expect("root validation context exists when roots are configured"),
            )?;
        }
        if let Some(protected_metadata_names) = settings.protected_metadata_names {
            config.protected_metadata_names =
                validate_protected_metadata_names(protected_metadata_names, source)?;
        }
        if let Some(level) = settings.windows_sandbox_level {
            config.windows_sandbox_level = WindowsSandboxLevel::parse(&level).ok_or_else(|| {
                SqueezyError::Config(format!(
                    "{source}: permissions.shell_sandbox.windows_sandbox_level invalid value {level:?}; expected disabled, restricted_token, or elevated"
                ))
            })?;
        }
        reject_duplicate_shell_roots(source, &config.read_roots, &config.write_roots)?;
        Ok(config)
    }
}

/// Valid env_allowlist patterns: exact names like `PATH`, or trailing-`*`
/// patterns like `LC_*`. We don't support `*FOO`, `FOO_*_BAR`, or any glob
/// containing characters the runtime matcher doesn't understand.
fn validate_env_allowlist_pattern(pattern: &str, source: &str) -> Result<()> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.env_allowlist contains empty pattern"
        )));
    }
    let star_count = trimmed.matches('*').count();
    if star_count > 1 || (star_count == 1 && !trimmed.ends_with('*')) {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.env_allowlist pattern {pattern:?} \
             only supports an exact name or a single trailing `*` (e.g. `LC_*`)"
        )));
    }
    if trimmed == "*" {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.env_allowlist pattern {pattern:?} \
             matches every variable and would preserve the entire host \
             environment; use an exact name or a non-empty prefix (e.g. `LC_*`)"
        )));
    }
    Ok(())
}

/// Valid sensitive_path_patterns: a leading path segment optionally followed
/// by trailing wildcards (`/**`, `/*`, or `*`). We disallow patterns whose
/// runtime base (everything up to the first wildcard) would be empty after
/// `sensitive_pattern_base`, since they degrade to "match every command".
fn validate_sensitive_path_pattern(pattern: &str, source: &str) -> Result<()> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.sensitive_path_patterns contains empty pattern"
        )));
    }
    if trimmed == "*" || trimmed == "**" {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.sensitive_path_patterns pattern {pattern:?} \
             matches every command and is refused"
        )));
    }
    // Strip any leading `/` so we look at the same base the runtime does.
    let body = trimmed.trim_start_matches('/');
    let base_end = body.find(['*', '?']).unwrap_or(body.len());
    if base_end == 0 {
        return Err(SqueezyError::Config(format!(
            "{source}: permissions.shell_sandbox.sensitive_path_patterns pattern {pattern:?} \
             must include a literal path prefix before any wildcard"
        )));
    }
    Ok(())
}

fn validate_shell_sandbox_roots(
    roots: Vec<String>,
    key: &str,
    source: &str,
    sensitive_patterns: &[String],
    root_validation: &ShellSandboxRootValidation,
) -> Result<Vec<PathBuf>> {
    let mut validated = Vec::new();
    for raw in roots {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} contains empty path"
            )));
        }
        let path = expand_home_path(PathBuf::from(trimmed));
        if !path.is_absolute() {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {trimmed:?} must be absolute"
            )));
        }
        let canonical = fs::canonicalize(&path).map_err(|err| {
            SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {} is not accessible: {err}",
                path.display()
            ))
        })?;
        if !canonical.is_dir() {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {} is not a directory",
                canonical.display()
            )));
        }
        if let Some(sensitive) =
            shell_root_sensitive_overlap(&canonical, sensitive_patterns, root_validation)
        {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {} is inside sensitive path {}",
                canonical.display(),
                sensitive.display()
            )));
        }
        if validated.contains(&canonical) {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.{key} path {} duplicates another configured root",
                canonical.display()
            )));
        }
        validated.push(canonical);
    }
    validated.sort();
    Ok(validated)
}

struct ShellSandboxRootValidation {
    workspace_root: PathBuf,
    home: Option<PathBuf>,
}

impl ShellSandboxRootValidation {
    fn new(workspace_root: &Path) -> Self {
        let workspace_root =
            fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| fs::canonicalize(&home).unwrap_or(home));
        Self {
            workspace_root,
            home,
        }
    }
}

fn reject_duplicate_shell_roots(
    source: &str,
    read_roots: &[PathBuf],
    write_roots: &[PathBuf],
) -> Result<()> {
    for read_root in read_roots {
        if write_roots.contains(read_root) {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox root {} appears in both read_roots and write_roots; write_roots already imply read access",
                read_root.display()
            )));
        }
    }
    Ok(())
}

fn validate_protected_metadata_names(names: Vec<String>, source: &str) -> Result<Vec<String>> {
    let mut validated = Vec::new();
    for raw in names {
        let name = raw.trim();
        if name.is_empty() {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.protected_metadata_names contains empty name"
            )));
        }
        if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return Err(SqueezyError::Config(format!(
                "{source}: permissions.shell_sandbox.protected_metadata_names name {raw:?} must be a single path segment"
            )));
        }
        let name = name.to_string();
        if !validated.contains(&name) {
            validated.push(name);
        }
    }
    if validated.is_empty() {
        tracing::warn!(
            target: "squeezy::permissions",
            source = %source,
            "permissions.shell_sandbox.protected_metadata_names is empty; metadata directory write protection is disabled"
        );
    }
    Ok(validated)
}

fn shell_root_sensitive_overlap(
    root: &Path,
    sensitive_patterns: &[String],
    root_validation: &ShellSandboxRootValidation,
) -> Option<PathBuf> {
    for pattern in sensitive_patterns {
        let base = sensitive_pattern_base(pattern);
        if base.is_empty() {
            continue;
        }
        let workspace_sensitive = root_validation.workspace_root.join(&base);
        if root.starts_with(&workspace_sensitive) {
            return Some(workspace_sensitive);
        }
        if let Some(home) = &root_validation.home {
            let home_sensitive = home.join(&base);
            if root.starts_with(&home_sensitive) {
                return Some(home_sensitive);
            }
        }
    }
    None
}

/// Returns the literal directory prefix of a sensitive-path glob pattern,
/// stripping any trailing wildcards (`*`, `/**`) and the leading `/`. Empty
/// output indicates that the pattern is purely a wildcard and should be
/// treated as having no enforceable prefix.
pub fn sensitive_pattern_base(pattern: &str) -> String {
    let trimmed = pattern
        .trim()
        .trim_end_matches('*')
        .trim_end_matches('/')
        .trim_end_matches("/**");
    trimmed.trim_start_matches('/').to_string()
}

fn default_shell_env_allowlist() -> Vec<String> {
    [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "TERM",
        "LANG",
        "TMPDIR",
        "TEMP",
        "TMP",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTFLAGS",
        "RUST_BACKTRACE",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "NIX_SSL_CERT_FILE",
        "LC_*",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_sensitive_path_patterns() -> Vec<String> {
    [
        ".ssh/**",
        ".aws/**",
        ".config/gh/**",
        ".netrc",
        ".gnupg/**",
        ".kube/**",
        ".docker/config.json",
        ".cargo/credentials*",
        ".npmrc",
        ".pypirc",
        ".env*",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_protected_metadata_names() -> Vec<String> {
    [".git", ".squeezy", ".agents"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PermissionScope {
    Read,
    Edit,
    Shell,
    IgnoredSearch,
    Web,
    /// External MCP tools. Treated as its own scope so the shell sandbox
    /// gating (network policy, plan-mode shell denial) does not accidentally
    /// extend to MCP calls without explicit opt-in.
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicy {
    pub mode: PermissionPolicyMode,
    pub read: PermissionMode,
    pub search: PermissionMode,
    pub edit: PermissionMode,
    pub shell: PermissionMode,
    pub ignored_search: PermissionMode,
    pub web: PermissionMode,
    pub mcp: PermissionMode,
    pub git: PermissionMode,
    pub compiler: PermissionMode,
    pub destructive: PermissionMode,
    pub shell_classifier: bool,
    pub ai_reviewer: AiReviewerConfig,
    pub shell_sandbox: ShellSandboxConfig,
    pub rules: Vec<PermissionRule>,
}

impl PermissionPolicy {
    pub fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        Self::from_settings_and_env(
            PermissionSettings::default(),
            "defaults",
            &env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            &mut var,
        )
        .expect("built-in permission defaults are valid")
    }

    fn from_settings_and_env(
        settings: PermissionSettings,
        source: &str,
        workspace_root: &Path,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Result<Self> {
        let legacy_defaults = settings.has_legacy_defaults();
        let custom_defaults = settings.custom.is_some();
        let legacy_compat = legacy_defaults && settings.mode.is_none();
        let mode = settings
            .mode
            .unwrap_or(if legacy_defaults || custom_defaults {
                PermissionPolicyMode::Custom
            } else {
                PermissionPolicyMode::Default
            });
        let ai_reviewer_settings = settings.ai_reviewer.clone();
        let ai_reviewer_allow_capabilities_configured = ai_reviewer_settings
            .as_ref()
            .is_some_and(|settings| settings.allow_capabilities.is_some());
        let shell_sandbox_settings = settings.shell_sandbox.clone();
        let shell_sandbox_network_configured = shell_sandbox_settings
            .as_ref()
            .is_some_and(|settings| settings.network.is_some());

        let mut policy = if legacy_compat {
            Self::legacy_compat_defaults()
        } else {
            Self::preset(mode)
        };
        policy.mode = mode;

        if mode == PermissionPolicyMode::Custom {
            policy.apply_legacy_defaults(&settings, legacy_compat);
            if let Some(custom) = &settings.custom {
                policy.apply_custom_defaults(custom);
            }
        }

        policy.read = parse_permission(var("SQUEEZY_READ_PERMISSION"), policy.read);
        policy.search = parse_permission(var("SQUEEZY_SEARCH_PERMISSION"), policy.search);
        policy.edit = parse_permission(var("SQUEEZY_EDIT_PERMISSION"), policy.edit);
        policy.shell = parse_permission(var("SQUEEZY_SHELL_PERMISSION"), policy.shell);
        policy.ignored_search = parse_permission(
            var("SQUEEZY_IGNORED_SEARCH_PERMISSION"),
            policy.ignored_search,
        );
        policy.web = parse_permission(var("SQUEEZY_WEB_PERMISSION"), policy.web);
        policy.mcp = parse_permission(var("SQUEEZY_MCP_PERMISSION"), policy.mcp);
        policy.git = parse_permission(var("SQUEEZY_GIT_PERMISSION"), policy.git);
        policy.compiler = parse_permission(var("SQUEEZY_COMPILER_PERMISSION"), policy.compiler);
        policy.destructive =
            parse_permission(var("SQUEEZY_DESTRUCTIVE_PERMISSION"), policy.destructive);
        policy.shell_classifier = parse_bool(
            var("SQUEEZY_SHELL_PERMISSION_CLASSIFIER"),
            settings.shell_classifier.unwrap_or(false),
        );

        policy.ai_reviewer = AiReviewerConfig::from_settings(ai_reviewer_settings, source)?;
        match mode {
            PermissionPolicyMode::AutoReview => {
                // Selecting Auto-review enables the reviewer (toggle it off by
                // choosing a different preset). The auto-approve set defaults to
                // the workspace-write capabilities but is respected when the user
                // has configured `allow_capabilities`, so the remit is tunable.
                policy.ai_reviewer.enabled = true;
                if !ai_reviewer_allow_capabilities_configured {
                    policy.ai_reviewer.allow_capabilities = auto_review_allow_capabilities();
                }
            }
            PermissionPolicyMode::FullAccess => {
                policy.ai_reviewer.enabled = false;
            }
            PermissionPolicyMode::Default | PermissionPolicyMode::Custom => {}
        }

        policy.shell_sandbox =
            ShellSandboxConfig::from_settings(shell_sandbox_settings, source, workspace_root)?;
        match mode {
            PermissionPolicyMode::Default | PermissionPolicyMode::AutoReview => {
                if !shell_sandbox_network_configured {
                    policy.shell_sandbox.network = ShellSandboxNetworkPolicy::AllowWhenApproved;
                }
            }
            PermissionPolicyMode::FullAccess => {
                policy.shell_sandbox.mode = ShellSandboxMode::Off;
                policy.shell_sandbox.network = ShellSandboxNetworkPolicy::AllowWhenApproved;
            }
            PermissionPolicyMode::Custom => {}
        }

        policy.rules = settings.rules;
        Ok(policy)
    }

    pub const fn mode_for(&self, scope: PermissionScope) -> PermissionMode {
        match scope {
            PermissionScope::Read => self.read,
            PermissionScope::Edit => self.edit,
            PermissionScope::Shell => self.shell,
            PermissionScope::IgnoredSearch => self.ignored_search,
            PermissionScope::Web => self.web,
            PermissionScope::Mcp => self.mcp,
        }
    }

    pub const fn mode_for_capability(&self, capability: PermissionCapability) -> PermissionMode {
        match capability {
            PermissionCapability::Read => self.read,
            PermissionCapability::Search => self.search,
            PermissionCapability::Edit => self.edit,
            PermissionCapability::Shell => self.shell,
            PermissionCapability::Network => self.web,
            PermissionCapability::Mcp => self.mcp,
            PermissionCapability::Git => self.git,
            PermissionCapability::Compiler => self.compiler,
            PermissionCapability::Destructive => self.destructive,
        }
    }

    pub fn apply_mode(&mut self, mode: PermissionPolicyMode) {
        let rules = std::mem::take(&mut self.rules);
        let shell_classifier = self.shell_classifier;
        let mut ai_reviewer = self.ai_reviewer.clone();
        let mut shell_sandbox = self.shell_sandbox.clone();
        let mut next = Self::preset(mode);
        next.rules = rules;
        next.shell_classifier = shell_classifier;
        ai_reviewer.enabled = next.ai_reviewer.enabled;
        ai_reviewer.allow_capabilities = next.ai_reviewer.allow_capabilities.clone();
        next.ai_reviewer = ai_reviewer;
        shell_sandbox.mode = next.shell_sandbox.mode;
        shell_sandbox.network = next.shell_sandbox.network;
        next.shell_sandbox = shell_sandbox;
        *self = next;
    }

    pub fn evaluate(&self, request: &PermissionRequest) -> PermissionVerdict {
        self.evaluate_with_extra(request, &[])
    }

    /// Like [`Self::evaluate`] but lets the caller layer additional rules on
    /// top of the configured ones. `extra` is treated as appended after
    /// `self.rules`, so the most recently added session rule wins over any
    /// rule from the on-disk config.
    pub fn evaluate_with_extra(
        &self,
        request: &PermissionRequest,
        extra: &[PermissionRule],
    ) -> PermissionVerdict {
        let matched_rule = self
            .rules
            .iter()
            .chain(extra.iter())
            .rev()
            .find(|rule| {
                wildcard_match(request.capability.as_str(), &rule.capability)
                    && wildcard_match(&request.target, &rule.target)
            })
            .cloned();
        if let Some(rule) = matched_rule {
            let (action, override_reason) =
                downgrade_unsafe_action(rule.action, request.capability, &rule.target);
            let reason = override_reason.unwrap_or_else(|| {
                rule.reason
                    .clone()
                    .unwrap_or_else(|| format!("matched {} permission rule", rule.source.as_str()))
            });
            // Silent is honored only when the resolved action is Deny: a
            // downgraded Allow that survives with silent=true would be outside
            // the documented policy shape (the loader already refuses silent
            // on non-Deny rules), and a Deny that was downgraded would also
            // drop silent.
            let silent = rule.silent && action == PermissionAction::Deny;
            return PermissionVerdict {
                action,
                reason,
                matched_rule: Some(rule),
                silent,
            };
        }
        let action = if path_request_targets_outside_workspace(request)
            && self.mode != PermissionPolicyMode::FullAccess
        {
            PermissionAction::Ask
        } else {
            self.mode_for_capability(request.capability)
        };
        PermissionVerdict {
            action,
            matched_rule: None,
            reason: format!(
                "default {} permission is {}",
                request.capability.as_str(),
                action.as_str()
            ),
            silent: false,
        }
    }
}

fn path_request_targets_outside_workspace(request: &PermissionRequest) -> bool {
    // `Shell` is included so a file-mutating shell command writing outside the
    // workspace (`sed -i ~/.bashrc`, `tee /etc/hosts`, `cp x /etc/y`) escalates
    // like the structured edit tools, instead of auto-allowing under the
    // workspace-write `shell` default. The shell permission request sets the
    // `outside_workspace` metadata in that case (chmod/ln/mv/touch classify as
    // `Edit`, which is already covered here).
    matches!(
        request.capability,
        PermissionCapability::Read | PermissionCapability::Edit | PermissionCapability::Shell
    ) && request
        .metadata
        .get("outside_workspace")
        .is_some_and(|value| value == "true")
}

/// Belt-and-suspenders safety: refuse to honor an Allow rule that targets the
/// `destructive` capability or whose `target` is functionally a "match
/// everything" wildcard. Returns the (possibly downgraded) action and an
/// explanatory reason when a downgrade happens.
fn downgrade_unsafe_action(
    action: PermissionAction,
    capability: PermissionCapability,
    target: &str,
) -> (PermissionAction, Option<String>) {
    if action == PermissionAction::Allow {
        if capability == PermissionCapability::Destructive {
            return (
                PermissionAction::Ask,
                Some(
                    "ignoring Allow rule on destructive capability; require explicit per-call approval"
                        .to_string(),
                ),
            );
        }
        if target_is_effectively_wildcard(target) {
            return (
                PermissionAction::Ask,
                Some(
                    "ignoring Allow rule with bare wildcard target; require a narrower target"
                        .to_string(),
                ),
            );
        }
    }
    (action, None)
}

/// True when a rule target is functionally identical to "match anything".
/// We refuse to load or persist Allow rules with such targets because they
/// undo the entire point of the permission system. The check is shared by
/// the on-disk load path (`permission_rules_value`), the session
/// persistence path (`install_persistent_rule`), and the runtime evaluator
/// (`downgrade_unsafe_action`) so the three layers cannot drift.
pub fn target_is_effectively_wildcard(target: &str) -> bool {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return true;
    }
    trimmed.chars().all(|ch| ch == '*' || ch.is_whitespace())
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        // Opt-in by default: ship the human-prompt `Default` preset, matching
        // peer agents (codex/clear-code) where the LLM reviewer is a choice the
        // user makes, not the shipped default. Users select `AutoReview` to turn
        // the reviewer on.
        Self::preset(PermissionPolicyMode::Default)
    }
}

impl PermissionPolicy {
    fn preset(mode: PermissionPolicyMode) -> Self {
        let mut shell_sandbox = ShellSandboxConfig {
            network: ShellSandboxNetworkPolicy::AllowWhenApproved,
            ..ShellSandboxConfig::default()
        };
        let mut ai_reviewer = AiReviewerConfig::default();
        let (read, search, edit, shell, ignored_search, web, mcp, git, compiler, destructive) =
            match mode {
                PermissionPolicyMode::Default | PermissionPolicyMode::Custom => (
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Ask,
                    PermissionMode::Ask,
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Ask,
                ),
                // Auto-review routes the workspace-write capabilities through the
                // reviewer (Ask) rather than auto-allowing them, so the reviewer
                // can actually adjudicate edit/shell/git/compiler. read/search stay
                // Allow; web/mcp stay Ask; destructive stays Ask (the reviewer may
                // deny it but never auto-approve it).
                PermissionPolicyMode::AutoReview => (
                    PermissionMode::Allow,
                    PermissionMode::Allow,
                    PermissionMode::Ask,
                    PermissionMode::Ask,
                    PermissionMode::Allow,
                    PermissionMode::Ask,
                    PermissionMode::Ask,
                    PermissionMode::Ask,
                    PermissionMode::Ask,
                    PermissionMode::Ask,
                ),
                PermissionPolicyMode::FullAccess => {
                    shell_sandbox.mode = ShellSandboxMode::Off;
                    (
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                        PermissionMode::Allow,
                    )
                }
            };
        if mode == PermissionPolicyMode::AutoReview {
            ai_reviewer.enabled = true;
            ai_reviewer.allow_capabilities = auto_review_allow_capabilities();
        }
        Self {
            mode,
            read,
            search,
            edit,
            shell,
            ignored_search,
            web,
            mcp,
            git,
            compiler,
            destructive,
            shell_classifier: false,
            ai_reviewer,
            shell_sandbox,
            rules: Vec::new(),
        }
    }

    fn legacy_compat_defaults() -> Self {
        Self {
            mode: PermissionPolicyMode::Custom,
            read: PermissionMode::Allow,
            search: PermissionMode::Allow,
            edit: PermissionMode::Allow,
            shell: PermissionMode::Ask,
            ignored_search: PermissionMode::Allow,
            web: PermissionMode::Ask,
            mcp: PermissionMode::Ask,
            git: PermissionMode::Ask,
            compiler: PermissionMode::Ask,
            destructive: PermissionMode::Ask,
            shell_classifier: false,
            ai_reviewer: AiReviewerConfig::default(),
            shell_sandbox: ShellSandboxConfig::default(),
            rules: Vec::new(),
        }
    }

    fn apply_legacy_defaults(&mut self, settings: &PermissionSettings, fan_out_shell: bool) {
        replace_if_some_value(&mut self.read, settings.read);
        replace_if_some_value(&mut self.search, settings.search);
        replace_if_some_value(&mut self.edit, settings.edit);
        if let Some(shell) = settings.shell {
            self.shell = shell;
            if fan_out_shell {
                self.git = shell;
                self.compiler = shell;
                self.destructive = shell;
            }
        }
        replace_if_some_value(&mut self.ignored_search, settings.ignored_search);
        replace_if_some_value(&mut self.web, settings.web);
        replace_if_some_value(&mut self.mcp, settings.mcp);
        replace_if_some_value(&mut self.git, settings.git);
        replace_if_some_value(&mut self.compiler, settings.compiler);
        replace_if_some_value(&mut self.destructive, settings.destructive);
    }

    fn apply_custom_defaults(&mut self, custom: &PermissionCustomSettings) {
        replace_if_some_value(&mut self.read, custom.read);
        replace_if_some_value(&mut self.search, custom.search);
        replace_if_some_value(&mut self.edit, custom.edit);
        replace_if_some_value(&mut self.shell, custom.shell);
        replace_if_some_value(&mut self.ignored_search, custom.ignored_search);
        replace_if_some_value(&mut self.web, custom.network);
        replace_if_some_value(&mut self.mcp, custom.mcp);
        replace_if_some_value(&mut self.git, custom.git);
        replace_if_some_value(&mut self.compiler, custom.compiler);
        replace_if_some_value(&mut self.destructive, custom.destructive);
    }
}

/// Default set of capabilities the Auto-review reviewer may auto-approve.
/// Includes the workspace-write capabilities (Edit/Shell/Git/Compiler) so the
/// reviewer actually adjudicates them; Destructive is intentionally excluded
/// (the reviewer may deny it but must never auto-approve it). Users can narrow
/// or widen this set via `permissions.ai_reviewer.allow_capabilities`.
fn auto_review_allow_capabilities() -> Vec<PermissionCapability> {
    vec![
        PermissionCapability::Read,
        PermissionCapability::Search,
        PermissionCapability::Network,
        PermissionCapability::Mcp,
        PermissionCapability::Edit,
        PermissionCapability::Shell,
        PermissionCapability::Git,
        PermissionCapability::Compiler,
    ]
}

fn parse_permission(value: Option<String>, default: PermissionMode) -> PermissionMode {
    value
        .as_deref()
        .and_then(PermissionMode::parse)
        .unwrap_or(default)
}

fn parse_session_mode(value: Option<String>, default: SessionMode) -> SessionMode {
    value
        .as_deref()
        .and_then(SessionMode::parse)
        .unwrap_or(default)
}

fn parse_session_mode_value(value: &str, source: &str, path: &str) -> Result<SessionMode> {
    SessionMode::parse(value).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid session mode {value:?}; expected plan or build"
        ))
    })
}

fn parse_session_resume_picker_value(
    value: &str,
    source: &str,
    path: &str,
) -> Result<SessionResumePicker> {
    SessionResumePicker::parse(value).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid resume picker value {value:?}; expected ask or never"
        ))
    })
}

fn parse_bool(value: Option<String>, default: bool) -> bool {
    value.as_deref().map_or(default, parse_enabled_bool)
}

fn parse_enabled_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn parse_disabled_bool(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "no" | "off" | "disabled")
    )
}

/// Tri-state bool parse for env overrides of `Option<bool>` settings.
/// Recognized truthy / falsy spellings map to `Some(true)` / `Some(false)`;
/// an empty or unset value yields `None` so the caller can fall back to the
/// TOML setting (which itself defaults to "leave the provider default").
fn parse_tristate_bool(value: Option<String>) -> Option<bool> {
    let raw = value?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => None,
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

fn parse_usize(value: Option<String>, default: usize) -> usize {
    value
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn parse_u64(value: Option<String>, default: u64) -> u64 {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub enabled: bool,
    pub endpoint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TelemetrySettings {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
}

impl TelemetrySettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["enabled", "endpoint"], source, path)?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            endpoint: string_value(table, "endpoint", source, &field(path, "endpoint"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.endpoint, next.endpoint);
    }
}

impl TelemetryConfig {
    pub fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        Self::from_settings_and_env(TelemetrySettings::default(), &mut var)
    }

    fn from_settings_and_env(
        settings: TelemetrySettings,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        let disabled = parse_disabled_bool(var("SQUEEZY_TELEMETRY").as_deref());
        let endpoint = var("SQUEEZY_TELEMETRY_ENDPOINT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(settings.endpoint)
            .unwrap_or_else(|| DEFAULT_TELEMETRY_ENDPOINT.to_string());
        Self {
            enabled: if disabled {
                false
            } else {
                settings.enabled.unwrap_or(true)
            },
            endpoint,
        }
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            endpoint: DEFAULT_TELEMETRY_ENDPOINT.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackConfig {
    pub enabled: bool,
    pub feedback_endpoint: String,
    pub report_endpoint: String,
    pub max_feedback_bytes: usize,
    pub max_report_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FeedbackSettings {
    pub enabled: Option<bool>,
    pub feedback_endpoint: Option<String>,
    pub report_endpoint: Option<String>,
    pub max_feedback_bytes: Option<usize>,
    pub max_report_bytes: Option<usize>,
}

impl FeedbackSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "feedback_endpoint",
                "report_endpoint",
                "max_feedback_bytes",
                "max_report_bytes",
            ],
            source,
            path,
        )?;
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?,
            feedback_endpoint: string_value(
                table,
                "feedback_endpoint",
                source,
                &field(path, "feedback_endpoint"),
            )?,
            report_endpoint: string_value(
                table,
                "report_endpoint",
                source,
                &field(path, "report_endpoint"),
            )?,
            max_feedback_bytes: usize_value(
                table,
                "max_feedback_bytes",
                source,
                &field(path, "max_feedback_bytes"),
            )?,
            max_report_bytes: usize_value(
                table,
                "max_report_bytes",
                source,
                &field(path, "max_report_bytes"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.enabled, next.enabled);
        replace_if_some(&mut self.feedback_endpoint, next.feedback_endpoint);
        replace_if_some(&mut self.report_endpoint, next.report_endpoint);
        replace_if_some(&mut self.max_feedback_bytes, next.max_feedback_bytes);
        replace_if_some(&mut self.max_report_bytes, next.max_report_bytes);
    }
}

impl FeedbackConfig {
    fn from_settings_and_env(
        settings: FeedbackSettings,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        let disabled = parse_disabled_bool(var("SQUEEZY_FEEDBACK").as_deref());
        let feedback_endpoint = var("SQUEEZY_FEEDBACK_ENDPOINT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(settings.feedback_endpoint)
            .unwrap_or_else(|| DEFAULT_FEEDBACK_ENDPOINT.to_string());
        let report_endpoint = var("SQUEEZY_REPORT_ENDPOINT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or(settings.report_endpoint)
            .unwrap_or_else(|| DEFAULT_REPORT_ENDPOINT.to_string());
        Self {
            enabled: if disabled {
                false
            } else {
                settings.enabled.unwrap_or(true)
            },
            feedback_endpoint,
            report_endpoint,
            max_feedback_bytes: settings
                .max_feedback_bytes
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_FEEDBACK_MAX_BYTES),
            max_report_bytes: settings
                .max_report_bytes
                .filter(|value| *value > 0)
                .unwrap_or(DEFAULT_REPORT_MAX_BYTES),
        }
    }
}

impl Default for FeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            feedback_endpoint: DEFAULT_FEEDBACK_ENDPOINT.to_string(),
            report_endpoint: DEFAULT_REPORT_ENDPOINT.to_string(),
            max_feedback_bytes: DEFAULT_FEEDBACK_MAX_BYTES,
            max_report_bytes: DEFAULT_REPORT_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RedactionSettings {
    pub custom_patterns: Option<Vec<String>>,
}

impl RedactionSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["custom_patterns"], source, path)?;
        Ok(Self {
            custom_patterns: string_array_value(
                table,
                "custom_patterns",
                source,
                &field(path, "custom_patterns"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.custom_patterns, next.custom_patterns);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionConfig {
    pub custom_patterns: Vec<String>,
}

impl RedactionConfig {
    fn from_settings(settings: RedactionSettings) -> Result<Self> {
        let config = Self {
            custom_patterns: settings.custom_patterns.unwrap_or_default(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        for (index, pattern) in self.custom_patterns.iter().enumerate() {
            Regex::new(pattern).map_err(|err| {
                SqueezyError::Config(format!(
                    "redaction.custom_patterns.{index}: invalid regex: {err}"
                ))
            })?;
        }
        Ok(())
    }

    pub fn redactor(&self) -> Result<Redactor> {
        Redactor::new(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedText {
    pub text: String,
    pub redactions: u64,
}

impl RedactedText {
    pub fn unchanged(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            redactions: 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Redactor {
    patterns: Vec<RedactionPattern>,
}

#[derive(Debug, Clone)]
struct RedactionPattern {
    kind: &'static str,
    regex: Regex,
}

impl Redactor {
    pub fn new(config: &RedactionConfig) -> Result<Self> {
        let mut patterns = Vec::new();
        for (kind, pattern) in DEFAULT_REDACTION_PATTERNS {
            patterns.push(RedactionPattern {
                kind,
                regex: Regex::new(pattern).map_err(|err| {
                    SqueezyError::Config(format!("built-in redaction pattern {kind}: {err}"))
                })?,
            });
        }
        for (index, pattern) in config.custom_patterns.iter().enumerate() {
            patterns.push(RedactionPattern {
                kind: "custom",
                regex: Regex::new(pattern).map_err(|err| {
                    SqueezyError::Config(format!(
                        "redaction.custom_patterns.{index}: invalid regex: {err}"
                    ))
                })?,
            });
        }
        Ok(Self { patterns })
    }

    pub fn redact(&self, text: &str) -> RedactedText {
        if text.is_empty() {
            return RedactedText::unchanged("");
        }

        // Track allocation lazily: keep `output` borrowed until a pattern
        // actually replaces something, then own the result. This keeps the
        // common no-match case allocation-free, which matters because the
        // redactor runs over every tool result, JSON arg, and model request.
        let mut output: Cow<'_, str> = Cow::Borrowed(text);
        let mut values = BTreeMap::<String, usize>::new();
        let mut redactions = 0u64;
        for pattern in &self.patterns {
            let next = pattern
                .regex
                .replace_all(output.as_ref(), |captures: &Captures<'_>| {
                    redactions += 1;
                    redact_capture(pattern.kind, captures, &mut values)
                });
            if let Cow::Owned(owned) = next {
                output = Cow::Owned(owned);
            }
        }
        match output {
            Cow::Borrowed(_) => RedactedText::unchanged(text),
            Cow::Owned(owned) => RedactedText {
                text: owned,
                redactions,
            },
        }
    }
}

/// Incrementally redacts a streaming text channel.
///
/// Emitting redacted token deltas naively is unsafe: a secret can be split
/// across two stream chunks, and a regex applied to either half misses it.
/// `StreamRedactor` keeps a tail buffer large enough to cover any realistic
/// single-line token plus a "hold" mode that suppresses output entirely
/// while a multi-line PEM block is open. Callers append text with [`push`]
/// (returning what is now safe to emit) and end with [`finish`] (returning
/// any remaining text after a final redaction pass).
///
/// [`push`]: StreamRedactor::push
/// [`finish`]: StreamRedactor::finish
#[derive(Debug)]
pub struct StreamRedactor {
    redactor: std::sync::Arc<Redactor>,
    buffer: String,
    redactions: u64,
    pem_open: bool,
}

/// Maximum number of bytes the stream redactor will keep buffered when no
/// multi-line pattern is open. Sized to comfortably exceed the longest
/// realistic single-line secret (long JWTs, bearer tokens, signed URLs).
const STREAM_TAIL_BYTES: usize = 1024;

const PEM_BEGIN: &str = "-----BEGIN";
const PEM_END: &str = "-----END";

impl StreamRedactor {
    pub fn new(redactor: std::sync::Arc<Redactor>) -> Self {
        Self {
            redactor,
            buffer: String::new(),
            redactions: 0,
            pem_open: false,
        }
    }

    /// Append `delta` to the internal buffer and return whatever portion is
    /// now safe to emit downstream. Returned text is fully redacted.
    pub fn push(&mut self, delta: &str) -> StreamChunk {
        if delta.is_empty() {
            return StreamChunk::empty();
        }
        self.buffer.push_str(delta);
        self.try_emit()
    }

    /// Flush any remaining buffered text after a final redaction pass.
    /// Returns the trailing redacted text and the total redactions seen
    /// since this redactor was created.
    pub fn finish(&mut self) -> StreamChunk {
        if self.buffer.is_empty() {
            return StreamChunk {
                text: String::new(),
                redactions: 0,
            };
        }
        let RedactedText { text, redactions } = self.redactor.redact(&self.buffer);
        self.redactions += redactions;
        self.buffer.clear();
        self.pem_open = false;
        StreamChunk { text, redactions }
    }

    pub fn total_redactions(&self) -> u64 {
        self.redactions
    }

    fn try_emit(&mut self) -> StreamChunk {
        // If we previously opened a PEM block, hold until we see END.
        if self.pem_open {
            if !self.buffer.contains(PEM_END) {
                return StreamChunk::empty();
            }
            self.pem_open = false;
        } else if let Some(begin) = self.buffer.find(PEM_BEGIN)
            && !self.buffer[begin..].contains(PEM_END)
        {
            self.pem_open = true;
            return StreamChunk::empty();
        }

        if self.buffer.len() <= STREAM_TAIL_BYTES {
            return StreamChunk::empty();
        }

        // Redaction markers are idempotent w.r.t. the built-in patterns, so
        // running the redactor over the whole buffer on each push is safe;
        // the previously-emitted prefix has been removed from `buffer`.
        let RedactedText {
            mut text,
            redactions,
        } = self.redactor.redact(&self.buffer);
        self.redactions += redactions;

        if text.len() <= STREAM_TAIL_BYTES {
            self.buffer = text;
            return StreamChunk {
                text: String::new(),
                redactions,
            };
        }

        let mut emit_end = text.len() - STREAM_TAIL_BYTES;
        emit_end = floor_char_boundary(&text, emit_end);
        emit_end = avoid_marker_split(&text, emit_end);
        if emit_end == 0 {
            self.buffer = text;
            return StreamChunk {
                text: String::new(),
                redactions,
            };
        }
        self.buffer = text.split_off(emit_end);
        StreamChunk { text, redactions }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamChunk {
    pub text: String,
    pub redactions: u64,
}

impl StreamChunk {
    pub fn empty() -> Self {
        Self {
            text: String::new(),
            redactions: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    idx = idx.min(s.len());
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn avoid_marker_split(text: &str, idx: usize) -> usize {
    let prefix = &text[..idx];
    let Some(open) = prefix.rfind("<redacted:") else {
        return idx;
    };
    if prefix[open..].contains('>') {
        return idx;
    }
    floor_char_boundary(text, open)
}

impl Default for Redactor {
    fn default() -> Self {
        RedactionConfig::default()
            .redactor()
            .expect("built-in redaction patterns must compile")
    }
}

fn redact_capture(
    kind: &'static str,
    captures: &Captures<'_>,
    values: &mut BTreeMap<String, usize>,
) -> String {
    let Some(full) = captures.get(0) else {
        return "<redacted:unknown#0 bytes=0>".to_string();
    };
    let value = captures.name("value").unwrap_or(full);
    let replacement = redaction_marker(kind, value.as_str(), values);
    if value.start() == full.start() && value.end() == full.end() {
        replacement
    } else {
        let relative_start = value.start() - full.start();
        let relative_end = value.end() - full.start();
        let full_text = full.as_str();
        format!(
            "{}{}{}",
            &full_text[..relative_start],
            replacement,
            &full_text[relative_end..]
        )
    }
}

fn redaction_marker(
    kind: &'static str,
    value: &str,
    values: &mut BTreeMap<String, usize>,
) -> String {
    let next = values.len() + 1;
    let ordinal = *values.entry(value.to_string()).or_insert(next);
    format!("<redacted:{kind}#{ordinal} bytes={}>", value.len())
}

const DEFAULT_REDACTION_PATTERNS: &[(&str, &str)] = &[
    // Order matters: `secret_assignment` runs first and consumes the value
    // half of `KEY=...`-style strings, so the per-provider patterns below
    // typically only fire on bare tokens that appear without an assignment
    // prefix (for example pasted command output). Keep that contract in
    // mind when reordering.
    //
    // The captured value excludes common trailing punctuation (`)`, `]`,
    // `}`, `>`, plus separators) so that surrounding shape is preserved in
    // shell output like `KEY=foo)` or markdown like `KEY=foo]`.
    (
        "secret_assignment",
        r#"(?i)\b[A-Z0-9_]*(?:API|AUTH|BEARER|CREDENTIAL|KEY|PASSWORD|SECRET|TOKEN)[A-Z0-9_]*\s*=\s*["']?(?P<value>[^\s"',;`)\]}>]+)"#,
    ),
    (
        "url_query",
        r#"(?i)[?&](?:access_token|api-key|api_key|apikey|code|key|signature|sig|token|x-amz-credential|x-amz-security-token|x-amz-signature)=(?P<value>[^&#\s]+)"#,
    ),
    (
        "url_userinfo",
        r#"(?i)https?://(?P<value>[^/\s:@]+:[^/\s@]+)@"#,
    ),
    (
        "bearer_token",
        r#"(?i)\bBearer\s+(?P<value>[A-Za-z0-9._~+/=-]{16,})\b"#,
    ),
    ("anthropic_key", r#"\bsk-ant-[A-Za-z0-9_-]{20,}\b"#),
    ("openai_key", r#"\bsk-[A-Za-z0-9][A-Za-z0-9_-]{20,}\b"#),
    ("google_key", r#"\bAIza[0-9A-Za-z_-]{20,}\b"#),
    ("github_token", r#"\bgh[pousr]_[A-Za-z0-9_]{20,}\b"#),
    ("aws_access_key", r#"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"#),
    (
        "jwt",
        r#"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b"#,
    ),
    (
        "private_key",
        r#"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----"#,
    ),
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WebSettings {
    pub exa_mcp_url: Option<String>,
    pub exa_api_key_env: Option<String>,
    pub parallel_mcp_url: Option<String>,
    pub parallel_api_key_env: Option<String>,
    /// Pluggable websearch backend selector. Valid values: `exa`,
    /// `parallel`.
    pub websearch_provider: Option<String>,
}

impl WebSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "exa_mcp_url",
                "exa_api_key_env",
                "parallel_mcp_url",
                "parallel_api_key_env",
                "websearch_provider",
            ],
            source,
            path,
        )?;
        Ok(Self {
            exa_mcp_url: string_value(table, "exa_mcp_url", source, &field(path, "exa_mcp_url"))?,
            exa_api_key_env: string_value(
                table,
                "exa_api_key_env",
                source,
                &field(path, "exa_api_key_env"),
            )?,
            parallel_mcp_url: string_value(
                table,
                "parallel_mcp_url",
                source,
                &field(path, "parallel_mcp_url"),
            )?,
            parallel_api_key_env: string_value(
                table,
                "parallel_api_key_env",
                source,
                &field(path, "parallel_api_key_env"),
            )?,
            websearch_provider: string_value(
                table,
                "websearch_provider",
                source,
                &field(path, "websearch_provider"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.exa_mcp_url, next.exa_mcp_url);
        replace_if_some(&mut self.exa_api_key_env, next.exa_api_key_env);
        replace_if_some(&mut self.parallel_mcp_url, next.parallel_mcp_url);
        replace_if_some(&mut self.parallel_api_key_env, next.parallel_api_key_env);
        replace_if_some(&mut self.websearch_provider, next.websearch_provider);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SkillsSettings {
    pub user_dir: Option<PathBuf>,
    pub compat_user_dir: Option<PathBuf>,
    /// Additional filesystem roots scanned during skill discovery. Each
    /// entry is treated like a user-level skills directory: skills loaded
    /// from these roots use [`SkillSource::ExtraRoot`] precedence, which
    /// sits above the personal `user_dir` but below project-local skills
    /// so a workspace's `.squeezy/skills/` still wins on name collisions.
    /// Use this to point at a shared mount, network drive, or vendored
    /// git submodule without standing up a marketplace or plugin host.
    pub extra_roots: Vec<PathBuf>,
    pub active_budget_chars: Option<usize>,
    pub active_body_cap_chars: Option<usize>,
    pub preamble_enabled: Option<bool>,
    pub preamble_budget_chars: Option<usize>,
    pub active_budget_mode: Option<SkillsBudgetMode>,
    pub preamble_budget_mode: Option<SkillsBudgetMode>,
    /// When `Some(true)`, restore the legacy behavior of inlining each
    /// activated skill's full body into the system prompt. The default
    /// (`None` / `Some(false)`) emits metadata-only blocks so the model
    /// pays for the body only when it explicitly calls `load_skill`.
    pub inline: Option<bool>,
    /// Opt-in to executing `hooks:` declared in `SKILL.md` frontmatter.
    /// `None`/`Some(false)` (the default) leaves the hook surface inert
    /// even though the parser accepts it. Setting this to `true` is an
    /// explicit acknowledgement that hook commands run as `sh -c` with
    /// the same privileges as the Squeezy process — the same trust
    /// boundary as the `shell` tool — and should only be enabled for
    /// skill catalogs the user controls.
    pub hooks_enabled: Option<bool>,
    pub config: Vec<SkillConfigEntry>,
}

impl SkillsSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "user_dir",
                "compat_user_dir",
                "extra_roots",
                "active_budget_chars",
                "active_body_cap_chars",
                "preamble_enabled",
                "preamble_budget_chars",
                "active_budget_mode",
                "preamble_budget_mode",
                "inline",
                "hooks_enabled",
                "config",
            ],
            source,
            path,
        )?;
        Ok(Self {
            user_dir: path_value(table, "user_dir", source, &field(path, "user_dir"))?,
            compat_user_dir: path_value(
                table,
                "compat_user_dir",
                source,
                &field(path, "compat_user_dir"),
            )?,
            extra_roots: path_array_value(
                table,
                "extra_roots",
                source,
                &field(path, "extra_roots"),
            )?,
            active_budget_chars: usize_value(
                table,
                "active_budget_chars",
                source,
                &field(path, "active_budget_chars"),
            )?,
            active_body_cap_chars: usize_value(
                table,
                "active_body_cap_chars",
                source,
                &field(path, "active_body_cap_chars"),
            )?,
            preamble_enabled: bool_value(
                table,
                "preamble_enabled",
                source,
                &field(path, "preamble_enabled"),
            )?,
            preamble_budget_chars: usize_value(
                table,
                "preamble_budget_chars",
                source,
                &field(path, "preamble_budget_chars"),
            )?,
            active_budget_mode: skills_budget_mode_value(
                table,
                "active_budget_mode",
                source,
                &field(path, "active_budget_mode"),
            )?,
            preamble_budget_mode: skills_budget_mode_value(
                table,
                "preamble_budget_mode",
                source,
                &field(path, "preamble_budget_mode"),
            )?,
            inline: bool_value(table, "inline", source, &field(path, "inline"))?,
            hooks_enabled: bool_value(
                table,
                "hooks_enabled",
                source,
                &field(path, "hooks_enabled"),
            )?,
            config: skill_config_entries_value(table, source, &field(path, "config"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.user_dir, next.user_dir);
        replace_if_some(&mut self.compat_user_dir, next.compat_user_dir);
        self.extra_roots.extend(next.extra_roots);
        replace_if_some(&mut self.active_budget_chars, next.active_budget_chars);
        replace_if_some(&mut self.active_body_cap_chars, next.active_body_cap_chars);
        replace_if_some(&mut self.preamble_enabled, next.preamble_enabled);
        replace_if_some(&mut self.preamble_budget_chars, next.preamble_budget_chars);
        replace_if_some(&mut self.active_budget_mode, next.active_budget_mode);
        replace_if_some(&mut self.preamble_budget_mode, next.preamble_budget_mode);
        replace_if_some(&mut self.inline, next.inline);
        replace_if_some(&mut self.hooks_enabled, next.hooks_enabled);
        self.config.extend(next.config);
    }
}

pub const DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS: usize = 4_000;
pub const DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS: usize = 16_000;
pub const DEFAULT_SKILLS_PREAMBLE_ENABLED: bool = true;
/// Must fit the fixed `<available_skills>` wrapper (intro + usage contract)
/// plus at least one catalog line; the contract alone is ~900 chars.
pub const DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS: usize = 1_200;
/// Default for `[skills] inline`. The metadata-only default keeps skill
/// bodies out of the system prompt; users that want the legacy behavior
/// of inlining each activated skill's body can set `[skills] inline = true`.
pub const DEFAULT_SKILLS_INLINE: bool = false;
/// Default for `[skills] hooks_enabled`. Lifecycle shell hooks declared
/// in `SKILL.md` frontmatter are inert unless the user opts in: hook
/// commands run with the same privileges as Squeezy itself, so this
/// stays off until explicitly enabled for a trusted skill catalog.
pub const DEFAULT_SKILLS_HOOKS_ENABLED: bool = false;
/// Default fraction of `model_context_window` (in percent) consumed by the
/// active and available-skills bundles when no explicit chars budget is set.
/// Matches the codex reference (`SKILL_METADATA_CONTEXT_WINDOW_PERCENT=2`).
pub const DEFAULT_SKILLS_BUDGET_CONTEXT_PERCENT: f32 = 2.0;
/// Conservative chars-per-token used when scaling a token-denominated context
/// window into a chars budget. Mirrors `CHARS_PER_TOKEN` in `ai_reviewer.rs`.
pub const SKILLS_CHARS_PER_TOKEN: u64 = 4;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillConfigEntry {
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub enabled: bool,
}

/// Selects how a skills budget is computed at render time.
///
/// `Chars` is an absolute cap and ignores the context window. `ContextPercent`
/// scales the budget to a fraction of `model_context_window` (converted to
/// chars via [`SKILLS_CHARS_PER_TOKEN`]), so larger-context models get
/// proportionally more room for skill instructions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SkillsBudgetMode {
    /// Absolute character cap, regardless of context window.
    Chars { chars: usize },
    /// Percentage of the model context window (0..=100), converted to chars
    /// via `SKILLS_CHARS_PER_TOKEN`.
    ContextPercent { percent: f32 },
}

impl Default for SkillsBudgetMode {
    fn default() -> Self {
        Self::ContextPercent {
            percent: DEFAULT_SKILLS_BUDGET_CONTEXT_PERCENT,
        }
    }
}

impl SkillsBudgetMode {
    /// Computes the effective character budget for this mode. `Chars(n)` is
    /// returned verbatim. `ContextPercent(p)` scales `model_context_window`
    /// (in tokens) to chars and applies `p%`; if the window is unknown, the
    /// caller's `fallback_chars` is used (typically the legacy
    /// `*_budget_chars` default).
    pub fn effective_chars(
        &self,
        model_context_window: Option<u64>,
        fallback_chars: usize,
    ) -> usize {
        match *self {
            Self::Chars { chars } => chars,
            Self::ContextPercent { percent } => {
                let Some(window) = model_context_window else {
                    return fallback_chars;
                };
                // Clamp to non-negative and bound the float math: at worst a
                // 200K-token window with percent=100 yields ~800k chars, well
                // under f32's safe integer range (~16M).
                let percent = percent.max(0.0);
                let chars_per_token = SKILLS_CHARS_PER_TOKEN as f32;
                let effective = (window as f32) * chars_per_token * percent / 100.0;
                effective.round().max(0.0) as usize
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillsConfig {
    pub user_dir: PathBuf,
    pub compat_user_dir: PathBuf,
    /// Additional filesystem roots walked during skill discovery. Their
    /// skills live at [`SkillSource::ExtraRoot`] precedence, above the
    /// personal `user_dir` and below project-local skills, so a workspace
    /// can still override a shared catalog by dropping a same-name skill
    /// in `.squeezy/skills/`. Non-existent entries are reported via
    /// `tracing::warn!` at discovery time and otherwise skipped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_roots: Vec<PathBuf>,
    pub active_budget_chars: usize,
    pub active_body_cap_chars: usize,
    pub preamble_enabled: bool,
    pub preamble_budget_chars: usize,
    /// Mode used to compute the active skills bundle budget at render time.
    /// Falls back to `Chars(active_budget_chars)` when only the legacy field
    /// is set in user settings.
    pub active_budget_mode: SkillsBudgetMode,
    /// Mode used to compute the available-skills preamble budget at render
    /// time. Falls back to `Chars(preamble_budget_chars)` when only the
    /// legacy field is set in user settings.
    pub preamble_budget_mode: SkillsBudgetMode,
    /// When `true`, the active-skills bundle inlines each skill's full
    /// body into the system prompt (the legacy behavior). The default
    /// (`false`) emits metadata-only blocks; the model fetches a body on
    /// demand via the `load_skill` tool.
    pub inline: bool,
    /// When `true`, `hooks:` blocks declared in skill frontmatter are
    /// registered against the agent hook registry on session start and
    /// fire during the matching lifecycle events. Defaults to `false`
    /// (the parser still accepts `hooks:` but the handlers stay dormant)
    /// because hook commands run unsandboxed via `sh -c` — the same
    /// trust boundary as the `shell` tool. Enable per-project when the
    /// skill catalog is fully trusted.
    pub hooks_enabled: bool,
    /// Token budget for the active model, copied from
    /// `context_compaction.model_context_window`. `None` keeps
    /// `ContextPercent` modes dormant and forces a fall-back to
    /// `*_budget_chars`.
    pub model_context_window: Option<u64>,
    pub config: Vec<SkillConfigEntry>,
}

impl SkillsConfig {
    pub fn from_env_vars(mut var: impl FnMut(&str) -> Option<String>) -> Self {
        Self::from_settings_and_env_vars(SkillsSettings::default(), &mut var)
    }

    fn from_settings_and_env_vars(
        settings: SkillsSettings,
        mut var: impl FnMut(&str) -> Option<String>,
    ) -> Self {
        // Legacy `*_budget_chars` keys keep working: if the user only set the
        // chars field (and not the new mode), treat it as a Chars-mode
        // override so behavior matches the pre-mode release exactly.
        let active_budget_chars = settings
            .active_budget_chars
            .unwrap_or(DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS);
        let preamble_budget_chars = settings
            .preamble_budget_chars
            .unwrap_or(DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS);
        let active_budget_mode = settings
            .active_budget_mode
            .or_else(|| {
                settings
                    .active_budget_chars
                    .map(|chars| SkillsBudgetMode::Chars { chars })
            })
            .unwrap_or_default();
        let preamble_budget_mode = settings
            .preamble_budget_mode
            .or_else(|| {
                settings
                    .preamble_budget_chars
                    .map(|chars| SkillsBudgetMode::Chars { chars })
            })
            .unwrap_or_default();
        Self {
            user_dir: expand_home_path(
                var("SQUEEZY_SKILLS_USER_DIR")
                    .map(PathBuf::from)
                    .or(settings.user_dir)
                    .unwrap_or_else(default_squeezy_skills_dir),
            ),
            compat_user_dir: expand_home_path(
                var("SQUEEZY_SKILLS_COMPAT_USER_DIR")
                    .map(PathBuf::from)
                    .or(settings.compat_user_dir)
                    .unwrap_or_else(default_agent_compat_skills_dir),
            ),
            extra_roots: settings
                .extra_roots
                .into_iter()
                .map(expand_home_path)
                .collect(),
            active_budget_chars,
            active_body_cap_chars: settings
                .active_body_cap_chars
                .unwrap_or(DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS),
            preamble_enabled: settings
                .preamble_enabled
                .unwrap_or(DEFAULT_SKILLS_PREAMBLE_ENABLED),
            preamble_budget_chars,
            active_budget_mode,
            preamble_budget_mode,
            inline: settings.inline.unwrap_or(DEFAULT_SKILLS_INLINE),
            hooks_enabled: settings
                .hooks_enabled
                .unwrap_or(DEFAULT_SKILLS_HOOKS_ENABLED),
            model_context_window: None,
            config: settings
                .config
                .into_iter()
                .map(|entry| SkillConfigEntry {
                    name: entry.name,
                    path: entry.path.map(expand_home_path),
                    enabled: entry.enabled,
                })
                .collect(),
        }
    }

    /// Computes the active skills bundle budget for the current context
    /// window. Falls through to `active_budget_chars` when the mode is
    /// `ContextPercent` but no window is configured.
    pub fn active_budget_effective_chars(&self) -> usize {
        self.active_budget_mode
            .effective_chars(self.model_context_window, self.active_budget_chars)
    }

    /// Computes the available-skills preamble budget for the current context
    /// window. Falls through to `preamble_budget_chars` when the mode is
    /// `ContextPercent` but no window is configured.
    pub fn preamble_budget_effective_chars(&self) -> usize {
        self.preamble_budget_mode
            .effective_chars(self.model_context_window, self.preamble_budget_chars)
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            user_dir: default_squeezy_skills_dir(),
            compat_user_dir: default_agent_compat_skills_dir(),
            extra_roots: Vec::new(),
            active_budget_chars: DEFAULT_SKILLS_ACTIVE_BUDGET_CHARS,
            active_body_cap_chars: DEFAULT_SKILLS_ACTIVE_BODY_CAP_CHARS,
            preamble_enabled: DEFAULT_SKILLS_PREAMBLE_ENABLED,
            preamble_budget_chars: DEFAULT_SKILLS_PREAMBLE_BUDGET_CHARS,
            active_budget_mode: SkillsBudgetMode::default(),
            preamble_budget_mode: SkillsBudgetMode::default(),
            inline: DEFAULT_SKILLS_INLINE,
            hooks_enabled: DEFAULT_SKILLS_HOOKS_ENABLED,
            model_context_window: None,
            config: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphConfig {
    pub languages: Vec<String>,
    pub max_file_bytes: u64,
    pub include_hidden: bool,
    pub require_indexing_signal: bool,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_classes: Vec<String>,
    pub exclude_classes: Vec<String>,
}

impl GraphConfig {
    fn from_settings(settings: GraphSettings) -> Self {
        Self {
            languages: settings
                .languages
                .unwrap_or_else(|| vec!["rust".to_string(), "python".to_string()]),
            max_file_bytes: settings.max_file_bytes.unwrap_or(1_000_000),
            include_hidden: settings.include_hidden.unwrap_or(false),
            require_indexing_signal: settings.require_indexing_signal.unwrap_or(true),
            include: settings.include.unwrap_or_default(),
            exclude: settings.exclude.unwrap_or_default(),
            include_classes: settings.include_classes.unwrap_or_default(),
            exclude_classes: settings.exclude_classes.unwrap_or_default(),
        }
    }
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self::from_settings(GraphSettings::default())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphSettings {
    pub languages: Option<Vec<String>>,
    pub max_file_bytes: Option<u64>,
    pub include_hidden: Option<bool>,
    pub require_indexing_signal: Option<bool>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub include_classes: Option<Vec<String>>,
    pub exclude_classes: Option<Vec<String>>,
}

impl GraphSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "languages",
                "max_file_bytes",
                "include_hidden",
                "require_indexing_signal",
                "include",
                "exclude",
                "include_classes",
                "exclude_classes",
            ],
            source,
            path,
        )?;
        Ok(Self {
            languages: string_array_value(table, "languages", source, &field(path, "languages"))?,
            max_file_bytes: u64_value(
                table,
                "max_file_bytes",
                source,
                &field(path, "max_file_bytes"),
            )?,
            include_hidden: bool_value(
                table,
                "include_hidden",
                source,
                &field(path, "include_hidden"),
            )?,
            require_indexing_signal: bool_value(
                table,
                "require_indexing_signal",
                source,
                &field(path, "require_indexing_signal"),
            )?,
            include: string_array_value(table, "include", source, &field(path, "include"))?,
            exclude: string_array_value(table, "exclude", source, &field(path, "exclude"))?,
            include_classes: string_array_value(
                table,
                "include_classes",
                source,
                &field(path, "include_classes"),
            )?,
            exclude_classes: string_array_value(
                table,
                "exclude_classes",
                source,
                &field(path, "exclude_classes"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.languages, next.languages);
        replace_if_some(&mut self.max_file_bytes, next.max_file_bytes);
        replace_if_some(&mut self.include_hidden, next.include_hidden);
        replace_if_some(
            &mut self.require_indexing_signal,
            next.require_indexing_signal,
        );
        replace_if_some(&mut self.include, next.include);
        replace_if_some(&mut self.exclude, next.exclude);
        replace_if_some(&mut self.include_classes, next.include_classes);
        replace_if_some(&mut self.exclude_classes, next.exclude_classes);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheConfig {
    pub root: Option<PathBuf>,
    pub tool_outputs: Option<PathBuf>,
}

impl CacheConfig {
    fn from_settings(settings: CacheSettings) -> Self {
        Self {
            root: settings.root,
            tool_outputs: settings.tool_outputs,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CacheSettings {
    pub root: Option<PathBuf>,
    pub tool_outputs: Option<PathBuf>,
}

impl CacheSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["root", "tool_outputs"], source, path)?;
        Ok(Self {
            root: path_value(table, "root", source, &field(path, "root"))?,
            tool_outputs: path_value(table, "tool_outputs", source, &field(path, "tool_outputs"))?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.root, next.root);
        replace_if_some(&mut self.tool_outputs, next.tool_outputs);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusVerbosity {
    Compact,
    Verbose,
}

impl StatusVerbosity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Verbose => "verbose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseVerbosity {
    Concise,
    Normal,
    Verbose,
}

impl ResponseVerbosity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Concise => "concise",
            Self::Normal => "normal",
            Self::Verbose => "verbose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputVerbosity {
    Compact,
    Normal,
    Verbose,
}

impl ToolOutputVerbosity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Normal => "normal",
            Self::Verbose => "verbose",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellDiffInline {
    /// Render unified-diff output from shell commands in full, bypassing the
    /// collapsed-card head/tail preview cap. Default — a `git diff` card is
    /// only useful when every hunk is visible.
    Full,
    /// Keep shell-produced diffs on the same head/tail preview budget as
    /// other shell output. For users who run `git diff` against large files
    /// often enough that uncapped inline diffs overwhelm the transcript.
    Folded,
}

impl ShellDiffInline {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Folded => "folded",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptDefault {
    Compact,
    Expanded,
}

impl TranscriptDefault {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Expanded => "expanded",
        }
    }
}

/// Controls whether the TUI wraps each frame draw in DEC mode 2026
/// (Begin/End Synchronized Update). Capable terminals (kitty, WezTerm,
/// Ghostty, iTerm2, Alacritty) flip the entire frame atomically, which
/// eliminates the cell-by-cell tearing visible during fast streaming.
/// The sequences are spec'd to be silently ignored by terminals that
/// do not implement them, so emitting them is safe by default.
///
/// `Auto` enables wrapping when capability is signalled via environment
/// (`KITTY_WINDOW_ID`, `WEZTERM_PANE`, `GHOSTTY_RESOURCES_DIR`,
/// `ALACRITTY_LOG`, `TERM_PROGRAM` matching iTerm/WezTerm/Ghostty, or
/// `TERM` containing `kitty`/`alacritty`/`wezterm`/`ghostty`). `Always`
/// forces wrapping on regardless of detection (useful for terminals
/// that do not advertise themselves but honour the sequence). `Never`
/// disables wrapping entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiSynchronizedOutput {
    Auto,
    Always,
    Never,
}

impl TuiSynchronizedOutput {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "always" | "on" | "true" => Some(Self::Always),
            "never" | "off" | "false" => Some(Self::Never),
            _ => None,
        }
    }
}

pub const DEFAULT_TUI_THEME_NAME: &str = "default";

pub const BUILTIN_TUI_THEME_NAMES: &[&str] =
    &["default", "bright", "fun", "catppuccin", "high-contrast"];

pub const DEFAULT_TUI_SPINNER_NAME: &str = "scintillate";

pub const BUILTIN_TUI_SPINNER_NAMES: &[&str] = &["twinkle", "scintillate", "drift"];

pub const TUI_THEME_COLOR_TOKENS: &[&str] = &[
    "palette.accent",
    "palette.secondary",
    "palette.red",
    "palette.green",
    "palette.yellow",
    "palette.blue",
    "palette.magenta",
    "palette.cyan",
    "ui.background",
    "ui.foreground",
    "ui.border",
    "ui.muted",
    "ui.quiet",
    "ui.footer",
    "ui.surface",
    "ui.prompt_bg",
    "syntax.keyword",
    "syntax.string",
    "syntax.comment",
    "syntax.literal",
    "syntax.function",
    "syntax.type",
    "syntax.operator",
    "syntax.variable",
    "status.ok",
    "status.warn",
    "status.err",
    "status.info",
    "transcript.user",
    "transcript.assistant",
    "transcript.tool",
    "transcript.system",
    "diff.added",
    "diff.removed",
    "diff.added_bg",
    "diff.removed_bg",
    "diff.context",
    "diff.hunk",
    "effects.shimmer",
    "separator.primary",
    "inline.code",
    "inline.model",
    "path.hint",
];

pub type TuiRgb = [u8; 3];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiThemeSettings {
    pub colors: BTreeMap<String, TuiRgb>,
}

impl TuiThemeSettings {
    fn merge(&mut self, next: Self) {
        for (token, rgb) in next.colors {
            self.colors.insert(token, rgb);
        }
    }
}

pub fn normalize_tui_theme_name(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    let canonical = match normalized.as_str() {
        "system" | "auto" | "dark" => DEFAULT_TUI_THEME_NAME,
        "light" => "bright",
        "mauve" => "catppuccin",
        "highcontrast" | "hc" => "high-contrast",
        other if is_valid_tui_theme_name(other) => other,
        _ => return None,
    };
    Some(canonical.to_string())
}

pub fn is_builtin_tui_theme_name(value: &str) -> bool {
    BUILTIN_TUI_THEME_NAMES.contains(&value)
}

pub fn normalize_tui_spinner_name(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    let canonical = match normalized.as_str() {
        "twinkle" | "twinkling" | "star" => "twinkle",
        "scintillate" | "scintillating" | "sparkle" => "scintillate",
        "drift" | "drifting" | "shooting" | "comet" => "drift",
        _ => return None,
    };
    Some(canonical.to_string())
}

pub fn is_tui_theme_color_token(value: &str) -> bool {
    TUI_THEME_COLOR_TOKENS.contains(&value)
}

pub fn is_valid_tui_theme_name(value: &str) -> bool {
    let len = value.len();
    len > 0
        && len <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Off-screen notification surface for turn-complete and approval-pending
/// events. Maps directly to bytes emitted to the controlling terminal:
/// `Bel` writes `\x07`, `Osc9` writes the iTerm-style OSC 9 desktop
/// notification escape, `Auto` picks OSC 9 when `$TERM_PROGRAM` matches a
/// known capable terminal and falls back to BEL otherwise. `Off` is the
/// default so a fresh install never makes noise the user did not ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationMethod {
    Off,
    Bel,
    Osc9,
    Auto,
}

impl NotificationMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Bel => "bel",
            Self::Osc9 => "osc9",
            Self::Auto => "auto",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "false" => Some(Self::Off),
            "bel" | "bell" => Some(Self::Bel),
            "osc9" | "osc-9" | "osc_9" => Some(Self::Osc9),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TuiConfig {
    pub tick_rate_ms: u64,
    pub status_verbosity: StatusVerbosity,
    pub response_verbosity: ResponseVerbosity,
    pub tool_output_verbosity: ToolOutputVerbosity,
    pub transcript_default: TranscriptDefault,
    /// DEC 2026 synchronized-output policy. `Auto` flips on for known
    /// capable terminals; `Always` forces it on; `Never` disables it.
    /// See [`TuiSynchronizedOutput`] for the capability heuristic.
    pub synchronized_output: TuiSynchronizedOutput,
    pub show_reasoning_usage: bool,
    /// Render-time grouping of adjacent same-tool same-status calls into
    /// one card (e.g. three back-to-back `read_file` calls become "Read 3
    /// files"). Independent of the push-time retry coalescer. Default
    /// `true`; flip to `false` to keep every tool call on its own row.
    pub coalesce_tool_runs: bool,
    /// Ordered list of status-line item identifiers. `None` means
    /// "use the built-in default list"; an empty list means the user
    /// deliberately disabled the detail line.
    pub status_line: Option<Vec<String>>,
    /// Color status-line items with their accent palette.
    /// Defaults to `true`.
    pub status_line_use_colors: bool,
    /// Active named TUI theme. Builtins are `default`, `bright`, `fun`,
    /// `catppuccin`, and `high-contrast`; user settings may add more names.
    pub theme: String,
    /// Working-status spinner style: `twinkle`, `scintillate`, or `drift`.
    pub spinner: String,
    /// User-defined or overridden theme colors, merged through the normal
    /// settings precedence chain.
    pub themes: BTreeMap<String, TuiThemeSettings>,
    /// Off-screen attention surface (OSC 9 desktop notification / BEL).
    /// Fires on turn-complete and approval-pending; default `Off`.
    pub desktop_notifications: NotificationMethod,
    /// Mirror the in-memory prompt-recall ring to a flat file on disk
    /// (default `~/.squeezy/prompt_history`, XDG-compatible). Off by
    /// default — history survives across sessions only when the user
    /// opts in, matching shell-history conventions and avoiding any
    /// surprise persisted plaintext for users who'd rather not have it.
    pub persist_prompt_history: bool,
    /// User-supplied key rebindings for the TUI composer / chat surface.
    /// Keyed by an action slug (e.g. `transcript_overlay`, `page_up`);
    /// the value is a key spec like `"Ctrl+t"` or `"PageUp"`. Unknown
    /// slugs and unparseable specs are surfaced by the TUI when
    /// `/keymap` is invoked.
    pub keymap: BTreeMap<String, String>,
    /// Whether `git diff`-style output from shell tools renders in full
    /// (default) or stays under the collapsed-card head/tail preview cap.
    pub shell_diff_inline: ShellDiffInline,
}

impl TuiConfig {
    fn from_settings(settings: TuiSettings) -> Self {
        Self {
            tick_rate_ms: settings.tick_rate_ms.unwrap_or(DEFAULT_TICK_RATE_MS),
            status_verbosity: settings
                .status_verbosity
                .unwrap_or(StatusVerbosity::Compact),
            response_verbosity: settings
                .response_verbosity
                .unwrap_or(ResponseVerbosity::Normal),
            tool_output_verbosity: settings
                .tool_output_verbosity
                .unwrap_or(ToolOutputVerbosity::Compact),
            transcript_default: settings
                .transcript_default
                .unwrap_or(TranscriptDefault::Compact),
            synchronized_output: settings
                .synchronized_output
                .unwrap_or(TuiSynchronizedOutput::Auto),
            show_reasoning_usage: settings.show_reasoning_usage.unwrap_or(true),
            coalesce_tool_runs: settings.coalesce_tool_runs.unwrap_or(true),
            status_line: settings.status_line,
            status_line_use_colors: settings.status_line_use_colors.unwrap_or(true),
            theme: settings
                .theme
                .unwrap_or_else(|| DEFAULT_TUI_THEME_NAME.to_string()),
            spinner: settings
                .spinner
                .unwrap_or_else(|| DEFAULT_TUI_SPINNER_NAME.to_string()),
            themes: settings.themes.unwrap_or_default(),
            desktop_notifications: settings
                .desktop_notifications
                .unwrap_or(NotificationMethod::Off),
            persist_prompt_history: settings.persist_prompt_history.unwrap_or(false),
            keymap: settings.keymap.unwrap_or_default(),
            shell_diff_inline: settings.shell_diff_inline.unwrap_or(ShellDiffInline::Full),
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self::from_settings(TuiSettings::default())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TuiSettings {
    pub tick_rate_ms: Option<u64>,
    pub status_verbosity: Option<StatusVerbosity>,
    pub response_verbosity: Option<ResponseVerbosity>,
    pub tool_output_verbosity: Option<ToolOutputVerbosity>,
    pub transcript_default: Option<TranscriptDefault>,
    pub synchronized_output: Option<TuiSynchronizedOutput>,
    pub show_reasoning_usage: Option<bool>,
    pub coalesce_tool_runs: Option<bool>,
    pub status_line: Option<Vec<String>>,
    pub status_line_use_colors: Option<bool>,
    pub theme: Option<String>,
    pub spinner: Option<String>,
    pub themes: Option<BTreeMap<String, TuiThemeSettings>>,
    pub desktop_notifications: Option<NotificationMethod>,
    pub persist_prompt_history: Option<bool>,
    pub keymap: Option<BTreeMap<String, String>>,
    pub shell_diff_inline: Option<ShellDiffInline>,
}

impl TuiSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "tick_rate_ms",
                "status_verbosity",
                "response_verbosity",
                "tool_output_verbosity",
                "transcript_default",
                "synchronized_output",
                "show_reasoning_usage",
                "coalesce_tool_runs",
                "status_line",
                "status_line_use_colors",
                "theme",
                "spinner",
                "themes",
                "desktop_notifications",
                "persist_prompt_history",
                "keymap",
                "shell_diff_inline",
            ],
            source,
            path,
        )?;
        Ok(Self {
            tick_rate_ms: u64_value(table, "tick_rate_ms", source, &field(path, "tick_rate_ms"))?,
            status_verbosity: status_verbosity_value(
                table,
                "status_verbosity",
                source,
                &field(path, "status_verbosity"),
            )?,
            response_verbosity: response_verbosity_value(
                table,
                "response_verbosity",
                source,
                &field(path, "response_verbosity"),
            )?,
            tool_output_verbosity: tool_output_verbosity_value(
                table,
                "tool_output_verbosity",
                source,
                &field(path, "tool_output_verbosity"),
            )?,
            transcript_default: transcript_default_value(
                table,
                "transcript_default",
                source,
                &field(path, "transcript_default"),
            )?,
            synchronized_output: tui_synchronized_output_value(
                table,
                "synchronized_output",
                source,
                &field(path, "synchronized_output"),
            )?,
            show_reasoning_usage: bool_value(
                table,
                "show_reasoning_usage",
                source,
                &field(path, "show_reasoning_usage"),
            )?,
            coalesce_tool_runs: bool_value(
                table,
                "coalesce_tool_runs",
                source,
                &field(path, "coalesce_tool_runs"),
            )?,
            status_line: string_array_value(
                table,
                "status_line",
                source,
                &field(path, "status_line"),
            )?,
            status_line_use_colors: bool_value(
                table,
                "status_line_use_colors",
                source,
                &field(path, "status_line_use_colors"),
            )?,
            theme: tui_theme_value(table, "theme", source, &field(path, "theme"))?,
            spinner: tui_spinner_value(table, "spinner", source, &field(path, "spinner"))?,
            themes: tui_themes_value(table, "themes", source, &field(path, "themes"))?,
            desktop_notifications: notification_method_value(
                table,
                "desktop_notifications",
                source,
                &field(path, "desktop_notifications"),
            )?,
            persist_prompt_history: bool_value(
                table,
                "persist_prompt_history",
                source,
                &field(path, "persist_prompt_history"),
            )?,
            keymap: string_map_value(table, "keymap", source, &field(path, "keymap"))?,
            shell_diff_inline: shell_diff_inline_value(
                table,
                "shell_diff_inline",
                source,
                &field(path, "shell_diff_inline"),
            )?,
        })
    }

    fn merge(&mut self, next: Self) {
        replace_if_some(&mut self.tick_rate_ms, next.tick_rate_ms);
        replace_if_some(&mut self.status_verbosity, next.status_verbosity);
        replace_if_some(&mut self.response_verbosity, next.response_verbosity);
        replace_if_some(&mut self.tool_output_verbosity, next.tool_output_verbosity);
        replace_if_some(&mut self.transcript_default, next.transcript_default);
        replace_if_some(&mut self.synchronized_output, next.synchronized_output);
        replace_if_some(&mut self.show_reasoning_usage, next.show_reasoning_usage);
        replace_if_some(&mut self.status_line, next.status_line);
        replace_if_some(
            &mut self.status_line_use_colors,
            next.status_line_use_colors,
        );
        replace_if_some(&mut self.theme, next.theme);
        merge_option(&mut self.themes, next.themes, merge_tui_theme_maps);
        replace_if_some(&mut self.desktop_notifications, next.desktop_notifications);
        replace_if_some(
            &mut self.persist_prompt_history,
            next.persist_prompt_history,
        );
        replace_if_some(&mut self.keymap, next.keymap);
        replace_if_some(&mut self.shell_diff_inline, next.shell_diff_inline);
    }
}

pub fn default_settings_path() -> PathBuf {
    if let Some(custom) = env::var_os("SQUEEZY_SETTINGS_PATH") {
        return PathBuf::from(custom);
    }
    if let Some(home) = home_squeezy_subpath("settings.toml") {
        return home;
    }
    if let Some(config) = dirs::config_dir() {
        return config.join("squeezy").join("settings.toml");
    }
    PathBuf::from(".squeezy/settings.toml")
}

/// Path of the on-disk prompt-recall ring backing `[tui]
/// .persist_prompt_history`. Prefers `$HOME/.squeezy/prompt_history`
/// for parity with `default_settings_path`; falls back to
/// `dirs::data_dir()/squeezy/prompt_history` (XDG-compatible:
/// `$XDG_DATA_HOME/squeezy/prompt_history` on Linux,
/// `%APPDATA%\squeezy\prompt_history` on Windows). Overridable with
/// `SQUEEZY_PROMPT_HISTORY_PATH` for tests and power users who keep
/// their dotfiles elsewhere.
pub fn default_prompt_history_path() -> PathBuf {
    if let Some(custom) = env::var_os("SQUEEZY_PROMPT_HISTORY_PATH") {
        return PathBuf::from(custom);
    }
    if let Some(home) = home_squeezy_subpath("prompt_history") {
        return home;
    }
    if let Some(data) = dirs::data_dir() {
        return data.join("squeezy").join("prompt_history");
    }
    PathBuf::from(".squeezy/prompt_history")
}

pub fn default_projects_dir() -> PathBuf {
    if let Some(custom) = env::var_os("SQUEEZY_PROJECTS_DIR") {
        return PathBuf::from(custom);
    }
    if let Some(home) = home_squeezy_subpath("projects") {
        return home;
    }
    if let Some(config) = dirs::config_dir() {
        return config.join("squeezy").join("projects");
    }
    PathBuf::from(".squeezy/projects")
}

/// Per-user global directory for Windows shell-sandbox state: the
/// capability-SID map (restricted-token tier, keyed by workspace path within
/// the file), and the elevated tier's sandbox-user secrets, setup marker, and
/// deny-read ACL state. It is global rather than per-workspace so the
/// machine-level elevated tier (local users + WFP filters) is provisioned and
/// torn down once per user, not duplicated — and a `--sandbox-teardown` in one
/// workspace does not delete users another workspace relies on. Windows-only in
/// practice, but defined cross-platform so callers need no `cfg`.
pub fn default_win_sandbox_state_dir() -> PathBuf {
    if let Some(custom) = env::var_os("SQUEEZY_WIN_SANDBOX_DIR") {
        return PathBuf::from(custom);
    }
    if let Some(home) = home_squeezy_subpath("win-sandbox") {
        return home;
    }
    if let Some(data) = dirs::data_dir() {
        return data.join("squeezy").join("win-sandbox");
    }
    PathBuf::from(".squeezy/win-sandbox")
}

fn home_squeezy_subpath(name: &str) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(".squeezy").join(name))
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        None
    }
}

pub fn repo_settings_id(root: impl AsRef<Path>) -> String {
    let root = root.as_ref();
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let display = canonical.display().to_string();
    let name = canonical
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_repo_settings_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "repo".to_string());
    format!("{name}-{:016x}", fnv1a64(display.as_bytes()))
}

pub fn per_repo_settings_path(root: impl AsRef<Path>) -> PathBuf {
    default_projects_dir()
        .join(repo_settings_id(root))
        .join("settings.toml")
}

fn sanitize_repo_settings_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn default_squeezy_skills_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_SQUEEZY_SKILLS_DIR))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SQUEEZY_SKILLS_DIR))
}

pub fn default_agent_compat_skills_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(DEFAULT_AGENT_COMPAT_SKILLS_DIR))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_AGENT_COMPAT_SKILLS_DIR))
}

fn expand_home_path(path: PathBuf) -> PathBuf {
    let Some(path_str) = path.to_str() else {
        return path;
    };
    if path_str == "~" {
        return env::var_os("HOME").map(PathBuf::from).unwrap_or(path);
    }
    if let Some(rest) = path_str.strip_prefix("~/") {
        return env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(rest))
            .unwrap_or(path);
    }
    path
}

/// Walks up the directory tree from `start` looking for `squeezy.toml`.
///
/// The starting directory is canonicalized so that `..` segments do not
/// confuse the walk and so that running from inside a symlinked checkout
/// resolves to the real workspace root. Falling back to the original path
/// when canonicalization fails (for example on a path that does not yet
/// exist) keeps tests and bare invocations working.
pub fn find_project_settings_path(start: impl AsRef<Path>) -> Option<PathBuf> {
    let start = start.as_ref();
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    if let Ok(canonical) = fs::canonicalize(&dir) {
        dir = canonical;
    }
    loop {
        let candidate = dir.join(PROJECT_SETTINGS_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub fn user_settings_template() -> &'static str {
    r#"# User-level Squeezy settings. Uncomment any key you want to override.
# Commented values are examples or defaults that apply when the key is absent.

[model]
# provider = "openai"          # openai | anthropic | google | azure_openai | bedrock | ollama
# profile = "balanced"         # cheap | balanced | strong
# model = "gpt-5.5"            # provider-specific model id; leave unset to use the provider default
# reasoning_effort = "medium"  # low | medium | high | xhigh; only sent to capable providers
# max_output_tokens = 64000    # optional output cap; unset means provider/model limit
# temperature = 0.2             # 0.0..2.0; absent means provider/model default
# top_p = 0.9                   # 0.0..1.0; absent means provider/model default
# seed = 42                     # non-negative integer; absent means provider/model default
# stop = []                     # stop sequences; empty/unset means provider/model default
# frequency_penalty = 0.0       # -2.0..2.0; absent means provider/model default
# presence_penalty = 0.0        # -2.0..2.0; absent means provider/model default
# stream_idle_timeout_ms = 300000 # fail a stalled model stream after 5m idle
# store_responses = false      # only honored by openai/azure_openai
# selection_version = 1        # maintained by the startup provider/model selector

[agent]
# exploration_graph = true  # graph-first planner for common navigation prompts

[session]
# mode = "build"              # build | plan
# resume_picker = "ask"       # ask | never
# log_dir = ".squeezy/sessions"
# log_retention_days = 30
# log_retention_archive_days = 30  # archived sessions deleted after this many days; 0 disables the archive sweep
# max_event_bytes = 65536
# max_session_bytes = 52428800

[context]
# compaction_enabled = true
# fallback_window_tokens = 128000  # window assumed when the model's real window is unknown
# max_context_tokens = 200000      # optional hard cap on the summarize threshold (omit to scale with the window)
# compaction_min_items = 16
# compaction_recent_items = 10
# compaction_max_summary_bytes = 12000
# repo_doc_max_bytes = 16384    # cap on AGENTS.md content stitched into base instructions (0 disables)
# user_memory_max_bytes = 8192  # cap on ~/.squeezy/MEMORY.md content stitched into base instructions (0 disables)
# enabled_mid_turn = true       # run the trim pass between LLM events within a turn
# model_context_window = 200000 # token budget for the active model; auto-derived from the model registry when unset
# effective_context_window_percent = 95  # % of the raw window treated as usable; summarize folds at this budget
# baseline_reserve_tokens = 12000        # tokens reserved off the effective window for system framing
# trim_at_percent = 40          # % of the effective window at which old tool output is trimmed in place
# warn_at_percent = 85          # % of the effective window at which the pre-summarize /pin nudge fires
# micro_compaction_enabled = true   # master switch for the trim tier
# micro_compaction_keep_recent = 5  # newest tool outputs the trim pass keeps verbatim
# strategy = "extractive"           # extractive | model_assisted | layered_fallback
# model_assisted_model = "gpt-5-nano"  # cheap model used when strategy != "extractive"
# model_assisted_max_output_tokens = 500
# model_assisted_timeout_secs = 30
# layered_fallback_extractive_threshold_tokens = 4000

[subagents]
# enabled = true
# explore_enabled = true
# explore_model = "gpt-5-nano" # optional cheap model override for the current provider
# max_concurrent = 20          # maximum parallel subagents per parent agent
# max_tool_calls_per_call = 10000
# max_tool_bytes_read_per_call = 1000000000
# max_search_files_per_call = 1000000
# max_model_rounds = 1000
# max_summary_tokens = 64000

# [providers.openai]
# api_key_env = "OPENAI_API_KEY"
# base_url = "https://api.openai.com/v1"
# default_model = "gpt-5.5"
# stream_idle_timeout_ms = 300000
# Per-provider turn-routing overrides (routing never crosses providers — these
# apply only when openai is the active provider; switching providers uses the
# other provider's own settings/defaults). Empty/unset = built-in defaults.
# cheap_model = "gpt-5.4-mini"   # model easy turns route TO (default: mini tier)
# judge_model = "gpt-5.4-mini"     # classifier model (keep it cheap; mini > nano)
# judge_prompt = "..."             # custom judge instructions (else built-in)
# expensive_models = "(?i)^(?!.*(nano|mini)).*"  # regex; reroute when the parent matches (default: skip this provider's cheap tiers)

# [providers.anthropic]
# api_key_env = "ANTHROPIC_API_KEY"
# base_url = "https://api.anthropic.com/v1"
# default_model = "claude-sonnet-4-6"
# stream_idle_timeout_ms = 300000
# cheap_model = "claude-haiku-4-5"
# judge_model = "claude-haiku-4-5"

# [routing]
# Auto-route easy turns to a cheaper model to cut cost. These toggles are
# GLOBAL; the cheap/judge MODELS are per-provider under [providers.<name>].
# Open this page in the TUI with `/router`.
# enabled = true               # master switch (same as `/router on|off`)
# heuristic = true             # static fast-path for obvious mechanical commands
# llm_judge = true             # ask the judge model on non-obvious turns
# follow_up_max_chars = 24     # short follow-ups inherit the prior turn's route
# judge_max_chars = 6000       # skip the judge for prompts longer than this

[permissions]
# mode = "auto_review"           # default | auto_review | full_access | custom
# auto_review allows workspace read/edit/search plus local shell/git/compiler;
# web, MCP, destructive actions, and outside-workspace paths still ask, with
# model-backed pre-review for read/search/network/MCP prompts.
# explicit default mode keeps the same capability defaults but disables AI review.
# Top-level capability keys below are legacy aliases. Prefer [permissions.custom]
# when mode = "custom".
# read = "allow"
# search = "allow"
# edit = "allow"
# shell = "allow"
# ignored_search = "allow"
# web = "ask"
# mcp = "ask"
# git = "allow"
# compiler = "allow"
# destructive = "ask"
# shell_classifier = false       # narrow LLM fallback for ambiguous shell commands (extra LLM call)
#
# [permissions.custom]           # used when permissions.mode = "custom"
# read = "allow"
# search = "allow"
# edit = "allow"
# shell = "allow"
# ignored_search = "allow"
# network = "ask"
# mcp = "ask"
# git = "allow"
# compiler = "allow"
# destructive = "ask"

# [permissions.ai_reviewer]
# enabled = true
# model = "gpt-5-mini"          # optional reviewer model override
# allow_capabilities = ["read", "search", "network", "mcp"]
# auto_review mode forces enabled=true and this allow_capabilities set.
# policy_file = ""              # optional local approval policy override
# timeout_secs = 15
# max_transcript_tokens = 4000  # sliding-window budget: keeps recent turns whole + summary of older
#
# Rule targets use prefix-tagged strings so different scopes don't collide.
# Known prefixes:
#   path:<rel-path>      - edit/write rules
#   domain:<host>        - network rules
#   search:<provider>    - web search rules
#   workspace:*          - read/search rules limited to workspace files
#   ignored:*            - read/search rules that include git-ignored files
#   tool:<name>          - catch-all per-tool rule
#   <cmd-prefix>:*       - shell/git/compiler rules (e.g. "cargo test:*", "rm:*")
# Allow rules on the `destructive` capability are refused at load time; keep
# them at `ask` or `deny`.
#
# [[permissions.rules]]
# capability = "network"
# target = "domain:docs.rs"
# action = "allow"
# source = "user"
#
# [[permissions.rules]]
# capability = "shell"
# target = "cargo test:*"
# action = "allow"
# source = "user"
#
# [[permissions.rules]]
# capability = "network"
# target = "shell:curl:*"
# action = "ask"
# source = "project"

# [permissions.shell_sandbox]
# mode = "best_effort"              # best_effort | required | off | external
# default/auto_review set network = "allow_when_approved" unless explicitly configured.
# network = "allow_when_approved"   # deny_by_default | allow_when_approved
# audit = true
# kill_grace_ms = 250
# env_allowlist = ["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "LANG", "TMPDIR", "TEMP", "TMP", "CARGO_HOME", "RUSTUP_HOME", "RUSTFLAGS", "RUST_BACKTRACE", "SSL_CERT_FILE", "SSL_CERT_DIR", "NIX_SSL_CERT_FILE", "LC_*"]
# read_roots = []                  # extra absolute directories shell may read
# write_roots = []                 # extra absolute directories shell may read/write
# protected_metadata_names = [".git", ".squeezy", ".agents"]
# sensitive_path_patterns = [".ssh/**", ".aws/**", ".config/gh/**", ".netrc", ".gnupg/**", ".kube/**", ".docker/config.json", ".cargo/credentials*", ".npmrc", ".pypirc", ".env*"]

[hardening]
# disable_core_dumps = true
# deny_debug_attach = true

[telemetry]
# enabled = true

[feedback]
# enabled = true
# feedback_endpoint = "https://squeezy-telemetry.esqueezy.workers.dev/v1/feedback"
# report_endpoint = "https://squeezy-telemetry.esqueezy.workers.dev/v1/report"
# max_feedback_bytes = 16384
# max_report_bytes = 2097152

# [redaction]
# custom_patterns = []

# [web]
# websearch_provider = "exa"          # "exa" or "parallel"
# exa_mcp_url = "https://mcp.exa.ai/mcp"
# exa_api_key_env = "EXA_API_KEY"
# parallel_mcp_url = "https://search.parallel.ai/mcp"
# parallel_api_key_env = "PARALLEL_API_KEY"

# [skills]
# user_dir = "~/.squeezy/skills"
# compat_user_dir = "~/.agents/skills"
# active_budget_chars = 4000          # legacy absolute cap; used only when active_budget_mode is unset
# active_body_cap_chars = 16000
# preamble_enabled = true
# preamble_budget_chars = 1200        # legacy absolute cap; used only when preamble_budget_mode is unset
# active_budget_mode = { context_percent = 2.0 }   # default; scales with [context].model_context_window
# preamble_budget_mode = { context_percent = 2.0 } # alternative: active_budget_mode = { chars = 4000 }
# inline = false                      # default; emit only metadata for active skills and let the model call load_skill on demand
# hooks_enabled = false               # default; opt in to running `hooks:` declared in SKILL.md frontmatter
#
# [[skills.config]]
# name = "example-skill"
# enabled = false

# [tools]
# checkpoints_enabled = false
# lazy_schema_loading = true
# `update_task_state` and `load_tool_schema` are always-core control tools
# and do not need to appear in `core`. See `DEFAULT_CORE_TOOL_NAMES` in
# `squeezy_core` for the authoritative default list.
# core = ["glob", "grep", "read_file", "read_tool_output", "write_file", "apply_patch", "shell", "decl_search", "definition_search", "diff_context", "downstream_flow", "hierarchy", "plan_patch", "read_slice", "reference_search", "repo_map", "symbol_context", "upstream_flow"]
# discoverable = []

[tui]
# tick_rate_ms = 50
# status_verbosity = "compact"   # compact | verbose
# response_verbosity = "normal"  # concise | normal | verbose
# tool_output_verbosity = "compact" # compact | normal | verbose
# transcript_default = "compact" # compact | expanded
# synchronized_output = "auto"  # auto | always | never (DEC 2026 atomic redraw)
# show_reasoning_usage = true
# persist_prompt_history = false  # mirror Up/Down prompt history to ~/.squeezy/prompt_history (XDG-compatible)

# [mcp.servers.docs]
# enabled = true
# transport = "stdio"       # stdio | http | sse
# command = "docs-mcp"
# args = []
# enabled_tools = ["lookup"]
# disabled_tools = []
#
# [mcp.servers.docs.permissions]
# default = "ask"
"#
}

pub fn project_settings_template() -> &'static str {
    r#"# Project-level Squeezy settings (committed alongside the project).
# Uncomment any key to override the built-in defaults shown after `=`.

[model]
# provider = "openai"          # openai | anthropic | google | azure_openai | bedrock | ollama
# profile = "balanced"         # cheap | balanced | strong
# model = "gpt-5.5"            # provider-specific model id; leave unset to use the provider default
# reasoning_effort = "medium"  # low | medium | high | xhigh; only sent to capable providers
# max_output_tokens = 64000    # optional output cap; unset means provider/model limit
# temperature = 0.2             # 0.0..2.0; absent means provider/model default
# top_p = 0.9                   # 0.0..1.0; absent means provider/model default
# seed = 42                     # non-negative integer; absent means provider/model default
# stop = []                     # stop sequences; empty/unset means provider/model default
# frequency_penalty = 0.0       # -2.0..2.0; absent means provider/model default
# presence_penalty = 0.0        # -2.0..2.0; absent means provider/model default
# stream_idle_timeout_ms = 300000 # fail a stalled model stream after 5m idle
# store_responses = false      # only honored by openai/azure_openai

[budgets]
# max_parallel_tools = 8
# max_tool_calls_per_turn = 64
# max_tool_bytes_read_per_turn = 20000000
# max_search_files_per_turn = 50000
# max_tool_result_bytes_per_round = 50000
# max_session_cost_usd_micros = 5000000
# cost_warn_percent = 85
# max_round_input_tokens = 200000  # pre-flight per-round input-token ceiling; unset = off (compact-first, then gate)

[agent]
# exploration_graph = true  # graph-first planner for common navigation prompts

[session]
# mode = "build"              # build | plan
# resume_picker = "ask"       # ask | never
# log_dir = ".squeezy/sessions"
# log_retention_days = 30
# log_retention_archive_days = 30  # archived sessions deleted after this many days; 0 disables the archive sweep
# max_event_bytes = 65536
# max_session_bytes = 52428800

[context]
# compaction_enabled = true
# fallback_window_tokens = 128000  # window assumed when the model's real window is unknown
# max_context_tokens = 200000      # optional hard cap on the summarize threshold (omit to scale with the window)
# compaction_min_items = 16
# compaction_recent_items = 10
# compaction_max_summary_bytes = 12000
# repo_doc_max_bytes = 16384    # cap on AGENTS.md content stitched into base instructions (0 disables)
# user_memory_max_bytes = 8192  # cap on ~/.squeezy/MEMORY.md content stitched into base instructions (0 disables)
# enabled_mid_turn = true       # run the trim pass between LLM events within a turn
# model_context_window = 200000 # token budget for the active model; auto-derived from the model registry when unset
# effective_context_window_percent = 95  # % of the raw window treated as usable; summarize folds at this budget
# baseline_reserve_tokens = 12000        # tokens reserved off the effective window for system framing
# trim_at_percent = 40          # % of the effective window at which old tool output is trimmed in place
# warn_at_percent = 85          # % of the effective window at which the pre-summarize /pin nudge fires
# micro_compaction_enabled = true   # master switch for the trim tier
# micro_compaction_keep_recent = 5  # newest tool outputs the trim pass keeps verbatim
# strategy = "extractive"           # extractive | model_assisted | layered_fallback
# model_assisted_model = "gpt-5-nano"  # cheap model used when strategy != "extractive"
# model_assisted_max_output_tokens = 500
# model_assisted_timeout_secs = 30
# layered_fallback_extractive_threshold_tokens = 4000

[subagents]
# enabled = true
# explore_enabled = true
# explore_model = "gpt-5-nano" # optional cheap model override for the current provider
# max_concurrent = 20          # maximum parallel subagents per parent agent
# max_tool_calls_per_call = 10000
# max_tool_bytes_read_per_call = 1000000000
# max_search_files_per_call = 1000000
# max_model_rounds = 1000
# max_summary_tokens = 64000

# [redaction]
# Add project-specific Rust regex patterns for secrets Squeezy should redact
# everywhere they appear in tool output, model requests, and UI surfaces.
# custom_patterns = []

[permissions]
# mode = "auto_review"           # default | auto_review | full_access | custom
# auto_review allows workspace read/edit/search plus local shell/git/compiler;
# web, MCP, destructive actions, and outside-workspace paths still ask, with
# model-backed pre-review for read/search/network/MCP prompts.
# explicit default mode keeps the same capability defaults but disables AI review.
# Use [permissions.custom] for granular custom-mode defaults.
#
# [permissions.custom]
# read = "allow"
# search = "allow"
# edit = "allow"
# shell = "allow"
# ignored_search = "allow"
# network = "ask"
# mcp = "ask"
# git = "allow"
# compiler = "allow"
# destructive = "ask"
#
# [permissions.ai_reviewer]
# enabled = true
# allow_capabilities = ["read", "search", "network", "mcp"]
# auto_review mode forces enabled=true and this allow_capabilities set.
#
# [[permissions.rules]]
# capability = "compiler"
# target = "cargo test:*"
# action = "allow"
# source = "project"
#
# [permissions.shell_sandbox]
# default/auto_review set network = "allow_when_approved" unless explicitly configured.
# network = "allow_when_approved"
# read_roots = []                  # shared absolute read-only shell roots
# write_roots = []                 # shared absolute read/write shell roots
# protected_metadata_names = [".git", ".squeezy", ".agents"]

[hardening]
# disable_core_dumps = true
# deny_debug_attach = true

# `[graph]` controls workspace indexing. `[mcp.servers.*]` configures
# external MCP tools that are discovered before each agent turn.

# [graph]
# languages = ["rust", "python"]
# max_file_bytes = 1000000
# include_hidden = false
# require_indexing_signal = true
# include = ["vendor/allowed/**"]
# exclude = ["fixtures/generated/**"]
# include_classes = ["lockfile"]
# exclude_classes = ["generated"]

[cache]
# Relative paths are resolved against the project root (the directory
# containing this squeezy.toml).
# tool_outputs = ".squeezy/tool_outputs"

# [tools]
# checkpoints_enabled = false
# lazy_schema_loading = true
# `update_task_state` and `load_tool_schema` are always-core control tools
# and do not need to appear in `core`. See `DEFAULT_CORE_TOOL_NAMES` in
# `squeezy_core` for the authoritative default list.
# core = ["glob", "grep", "read_file", "read_tool_output", "write_file", "apply_patch", "shell", "decl_search", "definition_search", "diff_context", "downstream_flow", "hierarchy", "plan_patch", "read_slice", "reference_search", "repo_map", "symbol_context", "upstream_flow"]
# discoverable = []

[tui]
# tick_rate_ms = 50
# status_verbosity = "compact"   # compact | verbose
# response_verbosity = "normal"  # concise | normal | verbose
# tool_output_verbosity = "compact" # compact | normal | verbose
# transcript_default = "compact" # compact | expanded
# synchronized_output = "auto"  # auto | always | never (DEC 2026 atomic redraw)
# show_reasoning_usage = true

# [mcp.servers.docs]
# enabled = true
# transport = "stdio"       # stdio | http | sse
# command = "docs-mcp"
# args = []
# enabled_tools = ["lookup"]
# disabled_tools = []
#
# [mcp.servers.docs.permissions]
# default = "ask"
"#
}

fn load_default_settings_sources() -> Result<(SettingsFile, Vec<String>, Vec<ConfigWarning>)> {
    let user_path = default_settings_path();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_path = find_project_settings_path(&cwd);
    let repo_root = project_path
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(cwd);
    let repo_path = per_repo_settings_path(repo_root);
    load_settings_from_paths(
        Some(user_path.as_path()),
        project_path.as_deref(),
        Some(repo_path.as_path()),
    )
}

/// A single tier's settings file as both its parsed form and its raw
/// `toml_edit` document. The document is what the writer mutates so saves
/// preserve user-authored comments and formatting. The path is what the UI
/// shows when the user asks "where does this value live?"
#[derive(Debug, Clone)]
pub struct TierSource {
    pub path: PathBuf,
    pub doc: toml_edit::DocumentMut,
}

impl TierSource {
    /// Whether this tier explicitly sets the leaf at `path`. Walks the parent
    /// tables and reports `true` only when the final segment is present.
    pub fn contains_path(&self, path: &[&str]) -> bool {
        if path.is_empty() {
            return false;
        }
        let (leaf, parents) = path.split_last().unwrap();
        let mut current = self.doc.as_table();
        for seg in parents {
            match current.get(seg) {
                Some(toml_edit::Item::Table(t)) => current = t,
                _ => return false,
            }
        }
        current.contains_key(leaf)
    }
}

/// The three tier files plus the effective merged config. Used by the config
/// screen to compute per-leaf inheritance badges.
///
/// Field naming intentionally mirrors the internal load order
/// (`user → project → repo`). User-facing labels in the TUI map differently:
/// `project` = the committed `./squeezy.toml` ("Repo" in the screen) and
/// `repo` = the per-machine `~/.squeezy/projects/<hash>/settings.toml`
/// ("Local" in the screen).
#[derive(Debug, Clone)]
pub struct SeparatedSources {
    pub user: Option<TierSource>,
    pub project: Option<TierSource>,
    pub repo: Option<TierSource>,
    pub user_path_default: PathBuf,
    pub project_path_default: PathBuf,
    pub repo_path_default: PathBuf,
}

/// Loads each tier separately so the UI can compute inheritance per leaf.
/// Reads each file independently (no merging here).
pub fn load_separated_settings_sources() -> Result<SeparatedSources> {
    let user_path = default_settings_path();
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_path = find_project_settings_path(&cwd);
    let repo_root = project_path
        .as_deref()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.clone());
    let repo_path = per_repo_settings_path(&repo_root);

    let user = load_tier_source(&user_path)?;
    let project = match project_path.as_ref() {
        Some(p) => load_tier_source(p)?,
        None => None,
    };
    let repo = load_tier_source(&repo_path)?;
    let project_path_default =
        project_path.unwrap_or_else(|| repo_root.join(PROJECT_SETTINGS_FILE));
    Ok(SeparatedSources {
        user,
        project,
        repo,
        user_path_default: user_path,
        project_path_default,
        repo_path_default: repo_path,
    })
}

fn load_tier_source(path: &Path) -> Result<Option<TierSource>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let doc = text.parse::<toml_edit::DocumentMut>().map_err(|err| {
                SqueezyError::Config(format!("toml_edit parse {}: {err}", path.display()))
            })?;
            Ok(Some(TierSource {
                path: path.to_path_buf(),
                doc,
            }))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Resolves which tier owns a field, using env > repo > project > user > default
/// precedence. Env wins because env-var overrides are applied after the merged
/// settings in `from_settings_and_env_vars`; repo wins next because it's the
/// last tier merged in `load_settings_from_paths`.
pub fn resolve_field_source(
    sources: &SeparatedSources,
    field: &config_schema::FieldMeta,
) -> config_schema::FieldSource {
    if let Some(var_name) = field.env_override
        && std::env::var(var_name).is_ok()
    {
        return config_schema::FieldSource::Env;
    }
    let path = field.toml_path;
    if let Some(repo) = &sources.repo
        && repo.contains_path(path)
    {
        return config_schema::FieldSource::Repo;
    }
    if let Some(project) = &sources.project
        && project.contains_path(path)
    {
        return config_schema::FieldSource::Project;
    }
    if let Some(user) = &sources.user
        && user.contains_path(path)
    {
        return config_schema::FieldSource::User;
    }
    config_schema::FieldSource::Default
}

fn load_settings_from_paths(
    user_path: Option<&Path>,
    project_path: Option<&Path>,
    repo_path: Option<&Path>,
) -> Result<(SettingsFile, Vec<String>, Vec<ConfigWarning>)> {
    let mut settings = SettingsFile::default();
    let mut sources = vec!["defaults".to_string()];
    let mut warnings = Vec::new();
    for (path, label) in [
        (user_path, "user"),
        (project_path, "project"),
        (repo_path, "repo"),
    ] {
        let Some(path) = path else { continue };
        if !path.is_file() {
            continue;
        }
        let source = format!("{label}:{}", path.display());
        let parsed = SettingsFile::from_toml_str(&fs::read_to_string(path)?, &source)?;
        let unknowns = take_unknown_fields();
        settings.merge(parsed);
        sources.push(source.clone());
        warnings.extend(config_warnings_from_unknown_fields(&source, unknowns));
    }
    Ok((settings, sources, warnings))
}

fn provider_setting(
    providers: &BTreeMap<String, ProviderSettings>,
    provider: &str,
    key: &str,
) -> Option<String> {
    let settings = providers.get(provider)?;
    let value = match key {
        "api_key_env" => settings.api_key_env.as_ref(),
        "api_key" => settings.api_key.as_ref(),
        "base_url" => settings.base_url.as_ref(),
        "default_model" => settings.default_model.as_ref(),
        "api_version" => settings.api_version.as_ref(),
        "region" => settings.region.as_ref(),
        "preset" => settings.preset.as_ref(),
        "vertex_project" => settings.vertex_project.as_ref(),
        "vertex_location" => settings.vertex_location.as_ref(),
        "route_style" => settings.route_style.as_ref(),
        "cloudflare_account_id" => settings.cloudflare_account_id.as_ref(),
        "cloudflare_gateway_id" => settings.cloudflare_gateway_id.as_ref(),
        "script" => settings.script.as_ref(),
        "organization" => settings.organization.as_ref(),
        "project" => settings.project.as_ref(),
        "service_tier" => settings.service_tier.as_ref(),
        "deployment_id" => settings.deployment_id.as_ref(),
        "cheap_model" => settings.cheap_model.as_ref(),
        "judge_model" => settings.judge_model.as_ref(),
        "judge_prompt" => settings.judge_prompt.as_ref(),
        _ => None,
    }?;
    Some(value.clone())
}

fn provider_setting_headers(
    providers: &BTreeMap<String, ProviderSettings>,
    provider: &str,
) -> Option<BTreeMap<String, String>> {
    providers.get(provider)?.headers.clone()
}

/// Resolve a `[providers.<section>.headers]` table from the first section
/// in `sections` that defines a non-empty map. Lets providers that accept
/// multiple TOML section aliases (e.g. Azure's `azure_openai` / `azure`)
/// share one header table without duplicating the lookup at every call
/// site.
fn provider_setting_headers_any(
    providers: &BTreeMap<String, ProviderSettings>,
    sections: &[&str],
) -> Option<BTreeMap<String, String>> {
    for section in sections {
        if let Some(headers) = providers
            .get(*section)
            .and_then(|settings| settings.headers.as_ref())
            && !headers.is_empty()
        {
            return Some(headers.clone());
        }
    }
    None
}

/// Resolve a typed boolean setting (currently `use_entra_id`) from the
/// first section in `sections` that defines it. Returns the first
/// non-`None` value so the higher-precedence alias wins when both
/// sections set the flag.
fn provider_setting_bool_any(
    providers: &BTreeMap<String, ProviderSettings>,
    sections: &[&str],
    key: &str,
) -> Option<bool> {
    for section in sections {
        let Some(settings) = providers.get(*section) else {
            continue;
        };
        let value = match key {
            "use_entra_id" => settings.use_entra_id,
            "use_oauth" => settings.use_oauth,
            _ => None,
        };
        if value.is_some() {
            return value;
        }
    }
    None
}

/// Resolve the Azure `deployment_name_map` from the first section in
/// `sections` that defines it. Empty (`{}`) is treated as "not set" so a
/// downstream config layer can still override; a missing field returns the
/// empty map so the runtime path defaults to passthrough.
fn provider_setting_deployment_name_map(
    providers: &BTreeMap<String, ProviderSettings>,
    sections: &[&str],
) -> BTreeMap<String, String> {
    for section in sections {
        if let Some(map) = providers
            .get(*section)
            .and_then(|settings| settings.deployment_name_map.as_ref())
            && !map.is_empty()
        {
            return map.clone();
        }
    }
    BTreeMap::new()
}

fn provider_setting_request_metadata(
    providers: &BTreeMap<String, ProviderSettings>,
    provider: &str,
) -> Option<BTreeMap<String, String>> {
    providers.get(provider)?.request_metadata.clone()
}

fn validate_provider_base_urls(provider: &ProviderConfig) -> Result<()> {
    match provider {
        ProviderConfig::OpenAi(cfg) => check_base_url_scheme(&cfg.base_url, "openai"),
        ProviderConfig::Anthropic(cfg) => check_base_url_scheme(&cfg.base_url, "anthropic"),
        ProviderConfig::Google(cfg) => check_base_url_scheme(&cfg.base_url, "google"),
        ProviderConfig::AzureOpenAi(cfg) => check_base_url_scheme(&cfg.base_url, "azure_openai"),
        ProviderConfig::Ollama(cfg) => check_base_url_scheme(&cfg.base_url, "ollama"),
        ProviderConfig::OpenAiCodex(cfg) => check_base_url_scheme(&cfg.base_url, "openai_codex"),
        ProviderConfig::GitHubCopilot(_) => Ok(()),
        ProviderConfig::OpenAiCompatible(cfg) => {
            check_base_url_scheme(&cfg.base_url, cfg.preset.as_str())
        }
        ProviderConfig::Bedrock(cfg) => match &cfg.base_url {
            Some(url) => check_base_url_scheme(url, "bedrock"),
            None => Ok(()),
        },
        // The faux provider runs entirely in-process; no base URL to
        // validate.
        ProviderConfig::Faux(_) => Ok(()),
    }
}

/// Refuses an HTTP `base_url` unless the host is a loopback identifier
/// (`localhost`, `127.0.0.0/8`, or `[::1]`). Anything else (LAN IPs, public
/// hostnames) must use HTTPS so a misconfigured config file cannot silently
/// exfiltrate API keys + prompt content to an attacker-controlled origin.
/// Independently of scheme, refuses any host that is a cloud-metadata
/// sentinel or IPv4/IPv6 link-local address. `https://169.254.169.254/...`,
/// `https://metadata.google.internal/...`, and `https://[fe80::1]/...`
/// would otherwise sail through the http-only filter and ship the Bearer
/// token to AWS IMDS / GCP metadata / Azure IMDS / a link-local
/// adversary on the LAN.
fn check_base_url_scheme(base_url: &str, section: &str) -> Result<()> {
    let trimmed = base_url.trim();
    // Extract the host irrespective of scheme so the metadata-sentinel
    // check fires for `https://` too (the original http-only filter
    // shipped Bearer tokens to AWS IMDS over TLS without complaint).
    let host_only = extract_host_only(trimmed);
    if let Some(host) = host_only.as_deref()
        && is_metadata_or_link_local_host(host)
    {
        return Err(SqueezyError::Config(format!(
            "providers.{section}.base_url host {host:?} resolves to a cloud-metadata or \
             link-local address (got {trimmed:?}); refusing to ship API keys or prompt \
             content to IMDS / metadata endpoints"
        )));
    }
    let Some(_rest) = trimmed.strip_prefix("http://") else {
        // Empty, https://, or any non-http scheme: the existing emptiness +
        // reachability checks elsewhere handle these. We only police http.
        return Ok(());
    };
    if host_only.as_deref().is_some_and(is_loopback_host) {
        return Ok(());
    }
    Err(SqueezyError::Config(format!(
        "providers.{section}.base_url must use https:// for non-loopback hosts (got {trimmed:?}); \
         API keys and prompt content would otherwise transit in cleartext"
    )))
}

/// Parse the host component out of a `base_url` string irrespective of
/// scheme. Returns `None` when the input lacks an `://` separator (we
/// only have a path) or is empty after the separator.
fn extract_host_only(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, rest)| rest)?;
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    if host.is_empty() {
        return None;
    }
    let stripped = host
        .strip_prefix('[')
        .and_then(|s| s.split_once(']'))
        .map(|(h, _)| h.to_string())
        .unwrap_or_else(|| host.split(':').next().unwrap_or("").to_string());
    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host == "::1" {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    false
}

/// Secondary env-var names accepted as fallbacks when the preset's
/// canonical `default_api_key_env` is empty. Lets squeezy honor the
/// out-of-band conventions mainstream platforms inject.
fn preset_api_key_env_aliases(preset: OpenAiCompatiblePreset) -> &'static [&'static str] {
    match preset {
        OpenAiCompatiblePreset::Vercel => &["VERCEL_OIDC_TOKEN"],
        OpenAiCompatiblePreset::CloudflareWorkersAi
        | OpenAiCompatiblePreset::CloudflareAiGateway => &["CLOUDFLARE_API_TOKEN"],
        OpenAiCompatiblePreset::DeepInfra => &["DEEPINFRA_TOKEN"],
        _ => &[],
    }
}

/// `true` when the literal host (no DNS resolution) names a cloud-metadata
/// sentinel or sits in an IPv4/IPv6 link-local range.
pub fn is_metadata_or_link_local_host(host: &str) -> bool {
    const METADATA_HOSTS: &[&str] = &["metadata.google.internal", "metadata", "metadata.google"];
    for needle in METADATA_HOSTS {
        if host.eq_ignore_ascii_case(needle) {
            return true;
        }
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return is_metadata_or_link_local_ip(&ip);
    }
    false
}

/// IP-address half of the metadata / link-local filter.
pub fn is_metadata_or_link_local_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            // Canonicalize IPv4-mapped addresses (e.g. `::ffff:169.254.169.254`)
            // and apply the IPv4 rules to the embedded address. Without this a
            // mapped IMDS/link-local literal slips past the narrow v6 checks
            // below — a well-known SSRF evasion that would otherwise re-open the
            // credential-exfiltration vector this filter exists to close.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_metadata_or_link_local_ip(&std::net::IpAddr::V4(v4));
            }
            let segments = v6.segments();
            // `fe80::/10` link-local, plus the entire `fc00::/7` IPv6
            // unique-local range (ULA). The AWS IPv6 IMDS sentinel
            // `fd00:ec2::254` sits inside `fc00::/7`, so the broad ULA test
            // subsumes it; without the range check a ULA-addressed internal
            // metadata/admin endpoint would slip past the narrow literal.
            (segments[0] & 0xffc0) == 0xfe80 || (segments[0] & 0xfe00) == 0xfc00
        }
    }
}

fn build_openai_compatible_config(
    preset: OpenAiCompatiblePreset,
    providers: &BTreeMap<String, ProviderSettings>,
    get_var: &mut dyn FnMut(&str) -> Option<String>,
) -> Result<ProviderConfig> {
    let section = preset.as_str();
    // Some presets ship secondary env-var names that mainstream tooling
    // honors out-of-band (Vercel injects `VERCEL_OIDC_TOKEN` into
    // every function runtime; Cloudflare's API token historically
    // shipped as both `CLOUDFLARE_API_KEY` and `CLOUDFLARE_API_TOKEN`;
    // DeepInfra docs reference both `DEEPINFRA_API_KEY` and
    // `DEEPINFRA_TOKEN`). When the user hasn't explicitly named an
    // `api_key_env` AND the preset's default env is empty in this
    // process, fall back to the first non-empty alias so an
    // out-of-the-box session works without per-shell env juggling.
    let api_key_env = provider_setting(providers, section, "api_key_env")
        .or_else(|| {
            let candidate = preset.default_api_key_env();
            if !candidate.is_empty()
                && get_var(candidate)
                    .map(|value| value.trim().is_empty())
                    .unwrap_or(true)
            {
                preset_api_key_env_aliases(preset)
                    .iter()
                    .find(|alias| {
                        get_var(alias)
                            .map(|value| !value.trim().is_empty())
                            .unwrap_or(false)
                    })
                    .map(|alias| (*alias).to_string())
            } else {
                None
            }
        })
        .or_else(|| {
            let candidate = preset.default_api_key_env();
            if candidate.is_empty() {
                None
            } else {
                Some(candidate.to_string())
            }
        })
        .ok_or_else(|| {
            SqueezyError::Config(format!(
                "providers.{section}.api_key_env is required for the {} preset",
                preset.display_name()
            ))
        })?;
    let base_url_override = get_var(&format!("{}_BASE_URL", section.to_ascii_uppercase()))
        .or_else(|| provider_setting(providers, section, "base_url"));
    let base_url = match (preset, base_url_override) {
        (_, Some(url)) => url,
        (OpenAiCompatiblePreset::Vertex, None) => {
            let project = get_var("VERTEX_PROJECT")
                .or_else(|| get_var("GOOGLE_CLOUD_PROJECT"))
                .or_else(|| provider_setting(providers, section, "vertex_project"))
                .ok_or_else(|| {
                    SqueezyError::Config(
                        "providers.vertex.vertex_project (or VERTEX_PROJECT / GOOGLE_CLOUD_PROJECT) is required for the Vertex AI preset"
                            .to_string(),
                    )
                })?;
            let location = get_var("VERTEX_LOCATION")
                .or_else(|| provider_setting(providers, section, "vertex_location"))
                .unwrap_or_else(|| DEFAULT_VERTEX_LOCATION.to_string());
            vertex_base_url(project.trim(), location.trim())
        }
        (_, None) => preset.default_base_url().to_string(),
    };
    if base_url.trim().is_empty() {
        return Err(SqueezyError::Config(format!(
            "providers.{section}.base_url is required for the {} preset",
            preset.display_name()
        )));
    }
    // Cloudflare presets carry `{account_id}` (and `{gateway_id}` for the
    // AI Gateway preset) placeholders in their default `base_url`
    // template; the LLM client substitutes them out of these fields
    // right before requests fire. Required values are validated up
    // front so misconfigurations surface at config-build time rather
    // than producing a confusing 404 against a literal `{account_id}`
    // URL. Aliases (`CLOUDFLARE_ACCOUNT_ID` env, `cloudflare_account_id`
    // TOML field) match the historical settings shape so existing
    // configs keep working.
    let (account_id, gateway_id) = match preset {
        OpenAiCompatiblePreset::CloudflareWorkersAi => {
            let account_id = get_var("CLOUDFLARE_ACCOUNT_ID")
                .or_else(|| provider_setting(providers, section, "cloudflare_account_id"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    SqueezyError::Config(
                        "providers.cloudflare_workers_ai.cloudflare_account_id (or CLOUDFLARE_ACCOUNT_ID) is required for the Cloudflare Workers AI preset"
                            .to_string(),
                    )
                })?;
            (Some(account_id), None)
        }
        OpenAiCompatiblePreset::CloudflareAiGateway => {
            let account_id = get_var("CLOUDFLARE_ACCOUNT_ID")
                .or_else(|| provider_setting(providers, section, "cloudflare_account_id"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    SqueezyError::Config(
                        "providers.cloudflare_ai_gateway.cloudflare_account_id (or CLOUDFLARE_ACCOUNT_ID) is required for the Cloudflare AI Gateway preset"
                            .to_string(),
                    )
                })?;
            let gateway_id = get_var("CLOUDFLARE_AI_GATEWAY_ID")
                .or_else(|| provider_setting(providers, section, "cloudflare_gateway_id"))
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| DEFAULT_CLOUDFLARE_AI_GATEWAY_ID.to_string());
            (Some(account_id), Some(gateway_id))
        }
        _ => (None, None),
    };
    let mut extra_headers = provider_setting_headers(providers, section).unwrap_or_default();
    // AI Gateway dual-auth: the upstream provider's API key flows in as the
    // standard `Authorization: Bearer …` (resolved by the provider), and an
    // optional gateway-level token flows in as `cf-aig-authorization`. Honor
    // `CF_AIG_TOKEN` as a convenience env so the gateway token does not have
    // to be pasted into a TOML `headers` table; user-supplied headers always
    // win to keep manual overrides possible.
    if preset == OpenAiCompatiblePreset::CloudflareAiGateway {
        let has_gateway_header = extra_headers
            .keys()
            .any(|key| key.eq_ignore_ascii_case("cf-aig-authorization"));
        if !has_gateway_header && let Some(token) = get_var("CF_AIG_TOKEN") {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                let value = format!("Bearer {trimmed}");
                // M-65 parity: the TOML `[headers]` path validates CR/LF at
                // config-load, but this env-sourced header bypassed that check
                // and only `trim()`s — an embedded CR/LF would survive to a
                // field-less reqwest error mid-stream. Validate here too.
                if http::HeaderValue::from_str(&value).is_err() {
                    return Err(SqueezyError::Config(
                        "CF_AIG_TOKEN contains bytes that cannot be sent as an HTTP header \
                         value (CR/LF and other control characters are forbidden); strip them"
                            .to_string(),
                    ));
                }
                extra_headers.insert("cf-aig-authorization".to_string(), value);
            }
        }
    }
    let transport = provider_transport_settings(providers, &[section]);
    let api_key = provider_setting(providers, section, "api_key");
    // Baseten dedicated deployments live behind per-deployment hosts
    // (`https://model-{deployment_id}.api.baseten.co/...`). The
    // placeholder substitution lives in the LLM client; here we just
    // collect the id so the runtime path has it. Reading from BOTH
    // `BASETEN_DEPLOYMENT_ID` env and TOML lets repo configs name a
    // sane default while operators per-shell override the deployment
    // they're testing.
    let deployment_id = if matches!(preset, OpenAiCompatiblePreset::Baseten) {
        get_var("BASETEN_DEPLOYMENT_ID")
            .or_else(|| provider_setting(providers, section, "deployment_id"))
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    } else {
        None
    };
    // The cf-aig-* knob surface only lights up for the AI Gateway preset.
    // Workers AI talks to Cloudflare directly and has no gateway between
    // squeezy and the model, so forwarding the same knobs there would
    // produce silent-no-op headers.
    let cf_ai_gateway = if matches!(preset, OpenAiCompatiblePreset::CloudflareAiGateway) {
        providers
            .get(section)
            .and_then(|settings| settings.cf_ai_gateway.clone())
    } else {
        None
    };
    // Vertex OAuth opt-in: explicit `use_oauth = true` in TOML wins;
    // otherwise infer from the environment — when `VERTEX_USE_OAUTH=1`
    // is set, or when the user supplied
    // `GOOGLE_APPLICATION_CREDENTIALS` (an ADC-style refresher) but no
    // static `VERTEX_ACCESS_TOKEN`. The LLM client is responsible for
    // actually wiring a `VertexOAuthSource` — squeezy-core only
    // surfaces the intent.
    let use_oauth = if matches!(preset, OpenAiCompatiblePreset::Vertex) {
        if let Some(value) =
            provider_setting_bool_any(providers, &[section, "vertex_ai"], "use_oauth")
        {
            value
        } else if let Some(flag) = get_var("VERTEX_USE_OAUTH") {
            matches!(
                flag.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        } else {
            get_var("GOOGLE_APPLICATION_CREDENTIALS").is_some()
                && get_var("VERTEX_ACCESS_TOKEN").is_none()
        }
    } else {
        false
    };
    // M-64: the `Custom` preset is the documented escape hatch for
    // self-hosted LiteLLM, vLLM, FastChat, and internal model gateways
    // (CT-3 in the shared audit). It carries no URL allow-list — every
    // other preset has a curated `default_base_url` that operators can
    // recognize at a glance, but Custom accepts whatever the user
    // supplies. A malicious project-local `./squeezy.toml` with
    // `model.provider = "openai_compatible"` + `base_url =
    // "https://attacker/v1"` + `api_key_env = "OPENAI_API_KEY"` is a
    // one-line credential-exfil primitive. Emit a startup warning so
    // operators see the resolved host before traffic flows; this is
    // explicitly *lightweight* (no interactive prompting, no refusal)
    // because the threat shape is project-config drift, not a
    // capability we need to gate.
    if matches!(preset, OpenAiCompatiblePreset::Custom) {
        tracing::warn!(
            target: "squeezy_core::config",
            "Custom preset bypasses URL allow-list; verify base_url={base_url} is trusted"
        );
    }
    Ok(ProviderConfig::OpenAiCompatible(OpenAiCompatibleConfig {
        preset,
        api_key_env,
        api_key,
        base_url,
        extra_headers,
        transport,
        account_id,
        gateway_id,
        deployment_id,
        cf_ai_gateway,
        use_oauth,
    }))
}

fn provider_settings_keys(provider: &ProviderConfig) -> &'static [&'static str] {
    match provider {
        ProviderConfig::OpenAi(_) => &["openai"],
        ProviderConfig::Anthropic(_) => &["anthropic"],
        ProviderConfig::Google(_) => &["google"],
        ProviderConfig::AzureOpenAi(_) => &["azure_openai", "azure"],
        ProviderConfig::Bedrock(_) => &["bedrock"],
        ProviderConfig::Ollama(_) => &["ollama"],
        ProviderConfig::OpenAiCodex(_) => &["openai_codex"],
        ProviderConfig::GitHubCopilot(_) => &["github_copilot", "github-copilot", "copilot"],
        ProviderConfig::OpenAiCompatible(config) => match config.preset {
            OpenAiCompatiblePreset::OpenRouter => &["openrouter"],
            OpenAiCompatiblePreset::Vercel => &["vercel"],
            OpenAiCompatiblePreset::PortKey => &["portkey"],
            OpenAiCompatiblePreset::Groq => &["groq"],
            OpenAiCompatiblePreset::XAi => &["xai"],
            OpenAiCompatiblePreset::DeepSeek => &["deepseek"],
            OpenAiCompatiblePreset::Vertex => &["vertex"],
            OpenAiCompatiblePreset::Mistral => &["mistral"],
            OpenAiCompatiblePreset::Together => &["together"],
            OpenAiCompatiblePreset::Fireworks => &["fireworks"],
            OpenAiCompatiblePreset::Cerebras => &["cerebras"],
            OpenAiCompatiblePreset::DeepInfra => &["deepinfra"],
            OpenAiCompatiblePreset::Baseten => &["baseten"],
            OpenAiCompatiblePreset::LMStudio => &["lmstudio"],
            OpenAiCompatiblePreset::VLlm => &["vllm"],
            OpenAiCompatiblePreset::LlamaCpp => &["llamacpp"],
            OpenAiCompatiblePreset::CloudflareWorkersAi => &["cloudflare_workers_ai"],
            OpenAiCompatiblePreset::CloudflareAiGateway => &["cloudflare_ai_gateway"],
            OpenAiCompatiblePreset::Custom => &["openai_compatible"],
        },
        ProviderConfig::Faux(_) => &["faux"],
    }
}

fn provider_u64_setting_any(
    providers: &BTreeMap<String, ProviderSettings>,
    provider_keys: &[&str],
    key: &str,
) -> Option<String> {
    provider_keys.iter().find_map(|provider| {
        let settings = providers.get(*provider)?;
        let value = match key {
            "stream_idle_timeout_ms" => settings.stream_idle_timeout_ms,
            _ => None,
        }?;
        Some(value.to_string())
    })
}

fn provider_transport_settings(
    providers: &BTreeMap<String, ProviderSettings>,
    names: &[&str],
) -> ProviderTransportConfig {
    let mut transport = ProviderTransportConfig::default();
    for name in names {
        let Some(settings) = providers.get(*name) else {
            continue;
        };
        if let Some(value) = settings.request_max_retries {
            transport.request_max_retries = value;
        }
        if let Some(value) = settings.stream_max_retries {
            transport.stream_max_retries = value;
        }
        if let Some(value) = settings.stream_idle_timeout_ms {
            transport.stream_idle_timeout_ms = value;
        }
    }
    transport
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSettings {
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpSettings {
    fn from_table(table: &toml::value::Table, source: &str, path: &str) -> Result<Self> {
        reject_unknown_keys(table, &["servers"], source, path)?;
        let Some(servers) = optional_table(table, "servers", source)? else {
            return Ok(Self::default());
        };
        let mut result = BTreeMap::new();
        for (name, value) in servers {
            let server_table = value.as_table().ok_or_else(|| {
                type_error(source, &field(&field(path, "servers"), name), "table")
            })?;
            result.insert(
                name.clone(),
                McpServerConfig::from_table(
                    name,
                    server_table,
                    source,
                    &field(&field(path, "servers"), name),
                )?,
            );
        }
        Ok(Self { servers: result })
    }

    fn merge(&mut self, next: Self) {
        for (name, server) in next.servers {
            match self.servers.entry(name) {
                Entry::Occupied(mut entry) => entry.get_mut().merge(server),
                Entry::Vacant(entry) => {
                    entry.insert(server);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpTransport {
    Stdio,
    Sse,
    Http,
}

impl McpTransport {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Sse => "sse",
            Self::Http => "http",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub enabled: bool,
    pub transport: McpTransport,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub timeout_ms: Option<u64>,
    /// Timeout applied to MCP tool discovery (`tools/list`, plus the implicit
    /// session bring-up on the first call). Falls back to `timeout_ms` when
    /// unset. Useful for servers that are slow to initialize but fast per
    /// call, or vice-versa.
    pub discovery_timeout_ms: Option<u64>,
    /// Timeout applied to MCP tool invocations and follow-on requests
    /// (`tools/call`, `resources/list`, `resources/read`). Falls back to
    /// `timeout_ms` when unset.
    pub tool_call_timeout_ms: Option<u64>,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub permissions: McpPermissionConfig,
    /// Name of the environment variable holding the bearer token for HTTP/SSE
    /// transports. Resolved at session start; missing env vars are skipped.
    pub bearer_token_env_var: Option<String>,
    /// Static HTTP headers attached to every request on HTTP/SSE transports.
    pub http_headers: BTreeMap<String, String>,
    /// HTTP headers whose values are read from environment variables at session
    /// start. Map key is the header name, value is the env var name. On
    /// conflict with `http_headers`, the env-sourced value wins.
    pub env_http_headers: BTreeMap<String, String>,
}

impl McpServerConfig {
    fn from_table(
        name: &str,
        table: &toml::value::Table,
        source: &str,
        path: &str,
    ) -> Result<Self> {
        reject_unknown_keys(
            table,
            &[
                "enabled",
                "transport",
                "command",
                "args",
                "url",
                "timeout_ms",
                "discovery_timeout_ms",
                "tool_call_timeout_ms",
                "enabled_tools",
                "disabled_tools",
                "env",
                "permissions",
                "bearer_token_env_var",
                "http_headers",
                "env_http_headers",
            ],
            source,
            path,
        )?;
        let transport = mcp_transport_value(table, "transport", source, &field(path, "transport"))?
            .unwrap_or(McpTransport::Stdio);
        let env = string_map_value(table, "env", source, &field(path, "env"))?.unwrap_or_default();
        let http_headers =
            string_map_value(table, "http_headers", source, &field(path, "http_headers"))?
                .unwrap_or_default();
        let env_http_headers = string_map_value(
            table,
            "env_http_headers",
            source,
            &field(path, "env_http_headers"),
        )?
        .unwrap_or_default();
        let permissions = optional_table(table, "permissions", source)?
            .map(|table| {
                McpPermissionConfig::from_table(name, table, source, &field(path, "permissions"))
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            enabled: bool_value(table, "enabled", source, &field(path, "enabled"))?.unwrap_or(true),
            transport,
            command: string_value(table, "command", source, &field(path, "command"))?,
            args: string_array_value(table, "args", source, &field(path, "args"))?
                .unwrap_or_default(),
            url: string_value(table, "url", source, &field(path, "url"))?,
            timeout_ms: u64_value(table, "timeout_ms", source, &field(path, "timeout_ms"))?,
            discovery_timeout_ms: u64_value(
                table,
                "discovery_timeout_ms",
                source,
                &field(path, "discovery_timeout_ms"),
            )?,
            tool_call_timeout_ms: u64_value(
                table,
                "tool_call_timeout_ms",
                source,
                &field(path, "tool_call_timeout_ms"),
            )?,
            enabled_tools: string_array_value(
                table,
                "enabled_tools",
                source,
                &field(path, "enabled_tools"),
            )?,
            disabled_tools: string_array_value(
                table,
                "disabled_tools",
                source,
                &field(path, "disabled_tools"),
            )?
            .unwrap_or_default(),
            env,
            permissions,
            bearer_token_env_var: string_value(
                table,
                "bearer_token_env_var",
                source,
                &field(path, "bearer_token_env_var"),
            )?,
            http_headers,
            env_http_headers,
        })
    }

    fn merge(&mut self, next: Self) {
        self.enabled = next.enabled;
        self.transport = next.transport;
        replace_if_some(&mut self.command, next.command);
        if !next.args.is_empty() {
            self.args = next.args;
        }
        replace_if_some(&mut self.url, next.url);
        replace_if_some(&mut self.timeout_ms, next.timeout_ms);
        replace_if_some(&mut self.discovery_timeout_ms, next.discovery_timeout_ms);
        replace_if_some(&mut self.tool_call_timeout_ms, next.tool_call_timeout_ms);
        replace_if_some(&mut self.enabled_tools, next.enabled_tools);
        if !next.disabled_tools.is_empty() {
            self.disabled_tools = next.disabled_tools;
        }
        if !next.env.is_empty() {
            self.env.extend(next.env);
        }
        self.permissions.merge(next.permissions);
        replace_if_some(&mut self.bearer_token_env_var, next.bearer_token_env_var);
        if !next.http_headers.is_empty() {
            self.http_headers.extend(next.http_headers);
        }
        if !next.env_http_headers.is_empty() {
            self.env_http_headers.extend(next.env_http_headers);
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPermissionConfig {
    pub default: Option<PermissionMode>,
    #[serde(default, skip)]
    pub default_source: Option<PermissionRuleSource>,
    pub rules: Vec<PermissionRule>,
}

impl McpPermissionConfig {
    fn from_table(
        server_name: &str,
        table: &toml::value::Table,
        source: &str,
        path: &str,
    ) -> Result<Self> {
        reject_unknown_keys(table, &["default", "rules"], source, path)?;
        let default = permission_value(table, "default", source, &field(path, "default"))?;
        let default_source = default.map(|_| default_permission_rule_source(source));
        let rules = mcp_permission_rules_value(server_name, table, source, &field(path, "rules"))?;
        Ok(Self {
            default,
            default_source,
            rules,
        })
    }

    fn merge(&mut self, next: Self) {
        if next.default.is_some() {
            self.default = next.default;
            self.default_source = next.default_source;
        }
        self.rules.extend(next.rules);
    }
}

fn mcp_permission_rules(servers: &BTreeMap<String, McpServerConfig>) -> Vec<PermissionRule> {
    let mut rules = Vec::new();
    for (server_name, server) in servers {
        if let Some(default) = server.permissions.default {
            rules.push(PermissionRule::new(
                "mcp",
                format!("{server_name}/*"),
                default,
                server
                    .permissions
                    .default_source
                    .unwrap_or(PermissionRuleSource::Project),
                Some(format!("default MCP policy for server {server_name}")),
            ));
        }
        rules.extend(server.permissions.rules.clone());
    }
    rules
}

fn providers_settings(
    table: &toml::value::Table,
    source: &str,
) -> Result<Option<BTreeMap<String, ProviderSettings>>> {
    let Some(providers) = optional_table(table, "providers", source)? else {
        return Ok(None);
    };
    let mut result = BTreeMap::new();
    for (name, value) in providers {
        let provider_table = value
            .as_table()
            .ok_or_else(|| type_error(source, &field("providers", name), "table"))?;
        result.insert(
            name.clone(),
            ProviderSettings::from_table(provider_table, source, &field("providers", name))?,
        );
    }
    Ok(Some(result))
}

fn parse_profiles_map(
    table: &toml::value::Table,
    source: &str,
) -> Result<Option<BTreeMap<String, SettingsFile>>> {
    let Some(profiles) = optional_table(table, "profiles", source)? else {
        return Ok(None);
    };
    let mut result = BTreeMap::new();
    for (name, value) in profiles {
        let inner = value
            .as_table()
            .ok_or_else(|| type_error(source, &field("profiles", name), "table"))?;
        let mut parsed =
            SettingsFile::from_toml_table(inner, &format!("{source}.profiles.{name}"))?;
        // Profiles are leaves: an inner `[profiles.<name>.profiles]` or
        // `profile = …` selector inside a named profile would invite
        // surprising recursion, so drop them silently here. The TOML parser
        // has already rejected truly-unknown keys via reject_unknown_keys.
        parsed.profiles = None;
        parsed.profile = None;
        result.insert(name.clone(), parsed);
    }
    Ok(Some(result))
}

thread_local! {
    /// Dotted paths of unknown fields seen during the most recent
    /// `SettingsFile::from_toml_str` call. The file loader clears this
    /// before parsing and drains it afterwards so the app can warn the
    /// user without rejecting or rewriting the settings file.
    static UNKNOWN_FIELDS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn reject_unknown_keys(
    table: &toml::value::Table,
    allowed: &[&str],
    source: &str,
    path: &str,
) -> Result<()> {
    // Pre-1.0 the schema is still moving; silently ignoring renamed or
    // removed fields lets users keep their old settings.toml around while
    // we iterate. The loader records `UNKNOWN_FIELDS` so startup can show
    // a warning in the transcript.
    for key in table.keys() {
        if !allowed.iter().any(|allowed| key == allowed) {
            let field_path = field(path, key);
            tracing::warn!(source, field = %field_path, "ignoring unknown config field");
            UNKNOWN_FIELDS.with(|cell| cell.borrow_mut().push(field_path));
        }
    }
    Ok(())
}

fn take_unknown_fields() -> Vec<String> {
    UNKNOWN_FIELDS.with(|cell| std::mem::take(&mut *cell.borrow_mut()))
}

fn config_warnings_from_unknown_fields(source: &str, fields: Vec<String>) -> Vec<ConfigWarning> {
    fields
        .into_iter()
        .map(|field| ConfigWarning {
            source: source.to_string(),
            field,
        })
        .collect()
}

fn optional_table<'a>(
    table: &'a toml::value::Table,
    key: &str,
    source: &str,
) -> Result<Option<&'a toml::value::Table>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_table()
            .map(Some)
            .ok_or_else(|| type_error(source, key, "table")),
    }
}

// SECURITY: `resolve_shell_escape` executes arbitrary shell commands as the
// invoking user, at config-load time. Any TOML string whose first byte is `!`
// (after any quoting the TOML parser already stripped) is passed verbatim to
// `/bin/sh -c` on Unix and `cmd.exe /C` on Windows. A malicious or hijacked
// `settings.toml` therefore has the same blast radius as a malicious shell
// rc-file: it runs before sandboxing, the agent loop, or the permission
// engine. This is intentional so users can wire in credential helpers like
// `!op read op://…` and `!gcloud auth …` without writing keys to disk, but it
// means settings files must be treated as code, not data. See
// `docs/internal/CONFIG_SHELL_ESCAPES.md` for the full guardrail note.
fn resolve_shell_escape(value: String, source: &str, path: &str) -> Result<String> {
    // Only the literal `!`-prefix form triggers execution; strings that merely
    // contain `!` anywhere else are passed through unchanged, e.g.
    // `prompt = "hello!"` or `regex = "[a-z]!\\d+"`.
    if !value.starts_with('!') {
        return Ok(value);
    }
    let command = &value[1..];
    if command.trim().is_empty() {
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: shell escape `!` is empty; expected `!<command>`"
        )));
    }

    #[cfg(windows)]
    let mut cmd = {
        let mut c = process::Command::new("cmd");
        c.args(["/C", command]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = process::Command::new("/bin/sh");
        c.args(["-c", command]);
        c
    };

    let output = cmd.output().map_err(|err| {
        SqueezyError::Config(format!(
            "{source}: {path}: failed to spawn shell escape `!{command}`: {err}"
        ))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        let code_part = output
            .status
            .code()
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| "no exit code (signal)".to_string());
        let stderr_part = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: shell escape `!{command}` failed with {code_part}{stderr_part}"
        )));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|err| {
        SqueezyError::Config(format!(
            "{source}: {path}: shell escape `!{command}` produced non-UTF-8 stdout: {err}"
        ))
    })?;

    // Trim a single trailing newline (the common shell-echo case) plus any
    // additional trailing whitespace, so credential helpers that emit
    // `secret\n` still round-trip cleanly.
    Ok(stdout.trim_end().to_string())
}

fn string_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<String>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let raw = value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error(source, path, "string"))?;
            resolve_shell_escape(raw, source, path).map(Some)
        }
    }
}

fn session_resume_picker_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<SessionResumePicker>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| type_error(source, path, "string"))?;
    parse_session_resume_picker_value(value, source, path).map(Some)
}

fn bool_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<bool>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| type_error(source, path, "boolean")),
    }
}

fn usize_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<usize>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let integer = positive_integer(value, source, path)?;
            usize::try_from(integer)
                .map(Some)
                .map_err(|_| SqueezyError::Config(format!("{source}: {path}: value is too large")))
        }
    }
}

fn u8_value(table: &toml::value::Table, key: &str, source: &str, path: &str) -> Result<Option<u8>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let integer = positive_integer(value, source, path)?;
            u8::try_from(integer)
                .map(Some)
                .map_err(|_| SqueezyError::Config(format!("{source}: {path}: value is too large")))
        }
    }
}

fn u32_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u32>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let integer = positive_integer(value, source, path)?;
            u32::try_from(integer)
                .map(Some)
                .map_err(|_| SqueezyError::Config(format!("{source}: {path}: value is too large")))
        }
    }
}

fn u8_nonnegative_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u8>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let integer = non_negative_integer(value, source, path)?;
            u8::try_from(integer)
                .map(Some)
                .map_err(|_| SqueezyError::Config(format!("{source}: {path}: value is too large")))
        }
    }
}

fn u64_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u64>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => Ok(Some(positive_integer(value, source, path)?)),
    }
}

fn u64_nonnegative_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u64>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => Ok(Some(non_negative_integer(value, source, path)?)),
    }
}

fn f32_range_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
    min: f32,
    max: f32,
) -> Result<Option<f32>> {
    match table.get(key) {
        None => Ok(None),
        Some(value) => {
            let number = value
                .as_float()
                .or_else(|| value.as_integer().map(|integer| integer as f64))
                .ok_or_else(|| type_error(source, path, "number"))?;
            if !number.is_finite() || number < f64::from(min) || number > f64::from(max) {
                return Err(SqueezyError::Config(format!(
                    "{source}: {path}: expected a number from {} to {}",
                    format_f32(min),
                    format_f32(max)
                )));
            }
            Ok(Some(number as f32))
        }
    }
}

fn percent_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<u8>> {
    let Some(value) = u8_nonnegative_value(table, key, source, path)? else {
        return Ok(None);
    };
    if (1..=100).contains(&value) {
        Ok(Some(value))
    } else {
        Err(SqueezyError::Config(format!(
            "{source}: {path}: expected an integer from 1 to 100"
        )))
    }
}

fn positive_integer(value: &toml::Value, source: &str, path: &str) -> Result<u64> {
    let Some(integer) = value.as_integer() else {
        return Err(type_error(source, path, "positive integer"));
    };
    if integer <= 0 {
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: expected a positive integer"
        )));
    }
    u64::try_from(integer)
        .map_err(|_| SqueezyError::Config(format!("{source}: {path}: expected a positive integer")))
}

fn non_negative_integer(value: &toml::Value, source: &str, path: &str) -> Result<u64> {
    let Some(integer) = value.as_integer() else {
        return Err(type_error(source, path, "non-negative integer"));
    };
    if integer < 0 {
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: expected a non-negative integer"
        )));
    }
    u64::try_from(integer).map_err(|_| {
        SqueezyError::Config(format!("{source}: {path}: expected a non-negative integer"))
    })
}

fn path_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<PathBuf>> {
    Ok(string_value(table, key, source, path)?.map(PathBuf::from))
}

fn path_array_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Vec<PathBuf>> {
    Ok(string_array_value(table, key, source, path)?
        .map(|values| values.into_iter().map(PathBuf::from).collect())
        .unwrap_or_default())
}

fn skills_budget_mode_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<SkillsBudgetMode>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let entry = value
        .as_table()
        .ok_or_else(|| type_error(source, path, "table"))?;
    reject_unknown_keys(entry, &["chars", "context_percent"], source, path)?;
    let chars = usize_value(entry, "chars", source, &field(path, "chars"))?;
    let context_percent = match entry.get("context_percent") {
        None => None,
        Some(value) => {
            let raw = value
                .as_float()
                .or_else(|| value.as_integer().map(|integer| integer as f64))
                .ok_or_else(|| type_error(source, &field(path, "context_percent"), "number"))?;
            if !raw.is_finite() || raw < 0.0 {
                return Err(SqueezyError::Config(format!(
                    "{source}: {}: expected a non-negative number",
                    field(path, "context_percent")
                )));
            }
            Some(raw as f32)
        }
    };
    match (chars, context_percent) {
        (Some(_), Some(_)) => Err(SqueezyError::Config(format!(
            "{source}: {path}: set exactly one of `chars` or `context_percent`",
        ))),
        (None, None) => Err(SqueezyError::Config(format!(
            "{source}: {path}: set either `chars` or `context_percent`",
        ))),
        (Some(chars), None) => Ok(Some(SkillsBudgetMode::Chars { chars })),
        (None, Some(percent)) => Ok(Some(SkillsBudgetMode::ContextPercent { percent })),
    }
}

fn skill_config_entries_value(
    table: &toml::value::Table,
    source: &str,
    path: &str,
) -> Result<Vec<SkillConfigEntry>> {
    let Some(value) = table.get("config") else {
        return Ok(Vec::new());
    };
    let Some(values) = value.as_array() else {
        return Err(type_error(source, path, "array of tables"));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let entry_path = format!("{path}.{index}");
            let entry = value
                .as_table()
                .ok_or_else(|| type_error(source, &entry_path, "table"))?;
            reject_unknown_keys(entry, &["name", "path", "enabled"], source, &entry_path)?;
            let enabled = bool_value(entry, "enabled", source, &field(&entry_path, "enabled"))?
                .ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "{source}: {}: missing field",
                        field(&entry_path, "enabled")
                    ))
                })?;
            Ok(SkillConfigEntry {
                name: string_value(entry, "name", source, &field(&entry_path, "name"))?,
                path: path_value(entry, "path", source, &field(&entry_path, "path"))?,
                enabled,
            })
        })
        .collect()
}

fn string_array_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<Vec<String>>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        return Err(type_error(source, path, "array of strings"));
    };
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let element_path = format!("{path}.{index}");
            let raw = value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error(source, &element_path, "string"))?;
            resolve_shell_escape(raw, source, &element_path)
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

fn string_map_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let Some(values) = value.as_table() else {
        return Err(type_error(source, path, "table of strings"));
    };
    values
        .iter()
        .map(|(key, value)| {
            let entry_path = field(path, key);
            let raw = value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error(source, &entry_path, "string"))?;
            let resolved = resolve_shell_escape(raw, source, &entry_path)?;
            Ok((key.clone(), resolved))
        })
        .collect::<Result<BTreeMap<_, _>>>()
        .map(Some)
}

fn permission_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<PermissionMode>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    PermissionMode::parse(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid permission mode {value:?}; expected allow, ask, or deny"
        ))
    })
}

fn permission_rules_value(
    table: &toml::value::Table,
    source: &str,
    path: &str,
) -> Result<Vec<PermissionRule>> {
    let Some(value) = table.get("rules") else {
        return Ok(Vec::new());
    };
    let rules = value
        .as_array()
        .ok_or_else(|| type_error(source, path, "array of tables"))?;
    rules
        .iter()
        .enumerate()
        .map(|value| {
            let rule_path = format!("{path}[{}]", value.0);
            let table = value
                .1
                .as_table()
                .ok_or_else(|| type_error(source, &rule_path, "table"))?;
            reject_unknown_keys(
                table,
                &["capability", "target", "action", "source", "reason", "silent"],
                source,
                &rule_path,
            )?;
            let capability = required_string_value(
                table,
                "capability",
                source,
                &field(&rule_path, "capability"),
            )?;
            if PermissionCapability::parse(&capability).is_none() && !capability.contains('*') {
                return Err(SqueezyError::Config(format!(
                    "{source}: {} invalid permission capability {capability:?}",
                    field(&rule_path, "capability")
                )));
            }
            let target =
                required_string_value(table, "target", source, &field(&rule_path, "target"))?;
            let action = permission_value(table, "action", source, &field(&rule_path, "action"))?
                .ok_or_else(|| {
                SqueezyError::Config(format!(
                    "{source}: {} missing required permission action",
                    field(&rule_path, "action")
                ))
            })?;
            if action == PermissionAction::Allow {
                if PermissionCapability::parse(&capability)
                    == Some(PermissionCapability::Destructive)
                {
                    return Err(SqueezyError::Config(format!(
                        "{source}: {rule_path}: refuse to load Allow rule on destructive capability; \
                         destructive actions must be approved per call or via a broader shell scope"
                    )));
                }
                if target_is_effectively_wildcard(&target) {
                    return Err(SqueezyError::Config(format!(
                        "{source}: {rule_path}: refuse to load Allow rule with bare wildcard target {target:?}; \
                         narrow the target to a specific path, host, or command prefix"
                    )));
                }
            }
            let source_value = string_value(table, "source", source, &field(&rule_path, "source"))?
                .as_deref()
                .and_then(PermissionRuleSource::parse)
                .unwrap_or_else(|| default_permission_rule_source(source));
            let reason = string_value(table, "reason", source, &field(&rule_path, "reason"))?;
            let silent =
                bool_value(table, "silent", source, &field(&rule_path, "silent"))?.unwrap_or(false);
            if silent && action != PermissionAction::Deny {
                return Err(SqueezyError::Config(format!(
                    "{source}: {rule_path}: silent = true is only valid on Deny rules; \
                     remove `silent` or set `action = \"deny\"`"
                )));
            }
            Ok(PermissionRule::new(capability, target, action, source_value, reason)
                .with_silent(silent))
        })
        .collect()
}

fn mcp_permission_rules_value(
    server_name: &str,
    table: &toml::value::Table,
    source: &str,
    path: &str,
) -> Result<Vec<PermissionRule>> {
    let Some(value) = table.get("rules") else {
        return Ok(Vec::new());
    };
    let rules = value
        .as_array()
        .ok_or_else(|| type_error(source, path, "array of tables"))?;
    rules
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let rule_path = format!("{path}[{index}]");
            let table = value
                .as_table()
                .ok_or_else(|| type_error(source, &rule_path, "table"))?;
            reject_unknown_keys(
                table,
                &["target", "action", "source", "reason", "silent"],
                source,
                &rule_path,
            )?;
            let target =
                required_string_value(table, "target", source, &field(&rule_path, "target"))?;
            let target = if target.starts_with(&format!("{server_name}/")) {
                target
            } else {
                format!("{server_name}/{target}")
            };
            let action = permission_value(table, "action", source, &field(&rule_path, "action"))?
                .ok_or_else(|| {
                    SqueezyError::Config(format!(
                        "{source}: {} missing required permission action",
                        field(&rule_path, "action")
                    ))
                })?;
            if action == PermissionAction::Allow && target_is_effectively_wildcard(&target) {
                return Err(SqueezyError::Config(format!(
                    "{source}: {rule_path}: refuse to load Allow rule with bare wildcard target {target:?}; \
                     narrow the target to a specific MCP server/tool"
                )));
            }
            let source_value = string_value(table, "source", source, &field(&rule_path, "source"))?
                .as_deref()
                .and_then(PermissionRuleSource::parse)
                .unwrap_or_else(|| default_permission_rule_source(source));
            let reason = string_value(table, "reason", source, &field(&rule_path, "reason"))?;
            let silent =
                bool_value(table, "silent", source, &field(&rule_path, "silent"))?.unwrap_or(false);
            if silent && action != PermissionAction::Deny {
                return Err(SqueezyError::Config(format!(
                    "{source}: {rule_path}: silent = true is only valid on Deny rules; \
                     remove `silent` or set `action = \"deny\"`"
                )));
            }
            Ok(PermissionRule::new("mcp", target, action, source_value, reason)
                .with_silent(silent))
        })
        .collect()
}

fn required_string_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<String> {
    string_value(table, key, source, path)?.ok_or_else(|| {
        SqueezyError::Config(format!("{source}: {path}: missing required string value"))
    })
}

fn default_permission_rule_source(source: &str) -> PermissionRuleSource {
    if source.starts_with("user:") {
        PermissionRuleSource::User
    } else {
        PermissionRuleSource::Project
    }
}

/// Minimal glob matcher for permission rule targets and capabilities.
///
/// Supports any number of `*` wildcards. Each `*` matches any (possibly empty)
/// run of characters; the prefix before the first `*` must anchor to the start
/// of `value` and the suffix after the last `*` must anchor to the end.
pub(crate) fn wildcard_match(value: &str, pattern: &str) -> bool {
    let value = value.trim();
    let pattern = pattern.trim();
    if pattern == value {
        return true;
    }
    if !pattern.contains('*') {
        return false;
    }
    let mut segments = pattern.split('*');
    let first = segments.next().unwrap_or_default();
    let last = segments.next_back().unwrap_or_default();
    if !value.starts_with(first) || !value.ends_with(last) {
        return false;
    }
    if first.len() + last.len() > value.len() {
        return false;
    }
    let mut cursor = first.len();
    let end = value.len() - last.len();
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        let Some(idx) = value
            .get(cursor..end)
            .and_then(|window| window.find(segment))
        else {
            return false;
        };
        cursor += idx + segment.len();
    }
    true
}

fn status_verbosity_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<StatusVerbosity>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "compact" => Ok(Some(StatusVerbosity::Compact)),
        "verbose" => Ok(Some(StatusVerbosity::Verbose)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid status verbosity {value:?}; expected compact or verbose"
        ))),
    }
}

fn response_verbosity_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<ResponseVerbosity>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "concise" => Ok(Some(ResponseVerbosity::Concise)),
        "normal" => Ok(Some(ResponseVerbosity::Normal)),
        "verbose" => Ok(Some(ResponseVerbosity::Verbose)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid response verbosity {value:?}; expected concise, normal, or verbose"
        ))),
    }
}

fn tool_output_verbosity_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<ToolOutputVerbosity>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "compact" => Ok(Some(ToolOutputVerbosity::Compact)),
        "normal" => Ok(Some(ToolOutputVerbosity::Normal)),
        "verbose" => Ok(Some(ToolOutputVerbosity::Verbose)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid tool output verbosity {value:?}; expected compact, normal, or verbose"
        ))),
    }
}

fn transcript_default_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<TranscriptDefault>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "compact" => Ok(Some(TranscriptDefault::Compact)),
        "expanded" => Ok(Some(TranscriptDefault::Expanded)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid transcript default {value:?}; expected compact or expanded"
        ))),
    }
}

fn shell_diff_inline_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<ShellDiffInline>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "full" => Ok(Some(ShellDiffInline::Full)),
        "folded" => Ok(Some(ShellDiffInline::Folded)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid shell diff inline {value:?}; expected full or folded"
        ))),
    }
}

fn tui_synchronized_output_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<TuiSynchronizedOutput>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    TuiSynchronizedOutput::parse(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid TUI synchronized output {value:?}; expected auto, always, or never"
        ))
    })
}

fn tui_theme_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<String>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    normalize_tui_theme_name(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid TUI theme {value:?}; expected a theme slug"
        ))
    })
}

fn tui_spinner_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<String>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    normalize_tui_spinner_name(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid TUI spinner {value:?}; expected twinkle, scintillate, or drift"
        ))
    })
}

fn tui_themes_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<BTreeMap<String, TuiThemeSettings>>> {
    let Some(value) = table.get(key) else {
        return Ok(None);
    };
    let themes = value
        .as_table()
        .ok_or_else(|| type_error(source, path, "table"))?;
    let mut out = BTreeMap::new();
    for (raw_name, value) in themes {
        let name = normalize_tui_theme_name(raw_name).ok_or_else(|| {
            SqueezyError::Config(format!(
                "{source}: {path}.{raw_name}: invalid TUI theme name"
            ))
        })?;
        let theme_table = value
            .as_table()
            .ok_or_else(|| type_error(source, &field(path, raw_name), "table"))?;
        reject_unknown_keys(theme_table, &["colors"], source, &field(path, raw_name))?;
        let Some(colors_value) = theme_table.get("colors") else {
            out.insert(name, TuiThemeSettings::default());
            continue;
        };
        let colors_table = colors_value
            .as_table()
            .ok_or_else(|| type_error(source, &field(&field(path, raw_name), "colors"), "table"))?;
        let mut colors = BTreeMap::new();
        collect_tui_theme_colors(
            colors_table,
            None,
            source,
            &field(&field(path, raw_name), "colors"),
            &mut colors,
        )?;
        out.insert(name, TuiThemeSettings { colors });
    }
    Ok(Some(out))
}

fn collect_tui_theme_colors(
    table: &toml::value::Table,
    prefix: Option<&str>,
    source: &str,
    path: &str,
    out: &mut BTreeMap<String, TuiRgb>,
) -> Result<()> {
    for (key, color_value) in table {
        let token = match prefix {
            Some(prefix) => format!("{prefix}.{key}"),
            None => key.to_string(),
        };
        let token_path = field(path, key);
        if let Some(child) = color_value.as_table() {
            collect_tui_theme_colors(child, Some(&token), source, &token_path, out)?;
            continue;
        }
        if !is_tui_theme_color_token(&token) {
            return Err(SqueezyError::Config(format!(
                "{source}: {token_path}: unknown TUI theme color token {token:?}"
            )));
        }
        out.insert(token, rgb_array_value(color_value, source, &token_path)?);
    }
    Ok(())
}

fn rgb_array_value(value: &toml::Value, source: &str, path: &str) -> Result<TuiRgb> {
    let array = value
        .as_array()
        .ok_or_else(|| type_error(source, path, "RGB array"))?;
    if array.len() != 3 {
        return Err(SqueezyError::Config(format!(
            "{source}: {path}: expected RGB array with exactly 3 integers"
        )));
    }
    let mut rgb = [0u8; 3];
    for (idx, item) in array.iter().enumerate() {
        let Some(value) = item.as_integer() else {
            return Err(type_error(source, path, "RGB integer"));
        };
        let Ok(channel) = u8::try_from(value) else {
            return Err(SqueezyError::Config(format!(
                "{source}: {path}: RGB channel {value} is outside 0..=255"
            )));
        };
        rgb[idx] = channel;
    }
    Ok(rgb)
}

fn notification_method_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<NotificationMethod>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    NotificationMethod::parse(&value).map(Some).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid notification method {value:?}; expected off, bel, osc9, or auto"
        ))
    })
}

fn reasoning_effort_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<ReasoningEffort>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    ReasoningEffort::parse(&value).ok_or_else(|| {
        SqueezyError::Config(format!(
            "{source}: {path}: invalid reasoning effort {value:?}; expected low, medium, high, or xhigh"
        ))
    }).map(Some)
}

fn tool_choice_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<String>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "auto" | "required" | "none" => Ok(Some(normalized)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid tool_choice {value:?}; expected auto, required, or none"
        ))),
    }
}

fn mcp_transport_value(
    table: &toml::value::Table,
    key: &str,
    source: &str,
    path: &str,
) -> Result<Option<McpTransport>> {
    let Some(value) = string_value(table, key, source, path)? else {
        return Ok(None);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "stdio" | "local" => Ok(Some(McpTransport::Stdio)),
        "sse" => Ok(Some(McpTransport::Sse)),
        "http" | "remote" => Ok(Some(McpTransport::Http)),
        _ => Err(SqueezyError::Config(format!(
            "{source}: {path}: invalid MCP transport {value:?}; expected stdio, sse, or http"
        ))),
    }
}

fn type_error(source: &str, path: &str, expected: &str) -> SqueezyError {
    SqueezyError::Config(format!("{source}: {path}: expected {expected}"))
}

fn field(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn replace_if_some<T>(target: &mut Option<T>, next: Option<T>) {
    if next.is_some() {
        *target = next;
    }
}

fn replace_if_some_value<T: Copy>(target: &mut T, next: Option<T>) {
    if let Some(next) = next {
        *target = next;
    }
}

fn merge_string_lists(target: &mut Option<Vec<String>>, next: Option<Vec<String>>) {
    let Some(next) = next else {
        return;
    };
    match target {
        Some(existing) => {
            for value in next {
                if !existing.contains(&value) {
                    existing.push(value);
                }
            }
        }
        None => *target = Some(next),
    }
}

fn merge_option<T>(target: &mut Option<T>, next: Option<T>, merge: impl FnOnce(&mut T, T)) {
    let Some(next) = next else {
        return;
    };
    match target {
        Some(existing) => merge(existing, next),
        None => *target = Some(next),
    }
}

fn merge_provider_maps(
    target: &mut Option<BTreeMap<String, ProviderSettings>>,
    next: Option<BTreeMap<String, ProviderSettings>>,
) {
    let Some(next) = next else {
        return;
    };
    let target = target.get_or_insert_with(BTreeMap::new);
    for (name, provider) in next {
        match target.entry(name) {
            Entry::Occupied(mut entry) => entry.get_mut().merge(provider),
            Entry::Vacant(entry) => {
                entry.insert(provider);
            }
        }
    }
}

fn model_limits_settings(
    table: &toml::value::Table,
    source: &str,
) -> Result<Option<BTreeMap<String, ModelLimitOverride>>> {
    let Some(limits) = optional_table(table, "model_limits", source)? else {
        return Ok(None);
    };
    let mut result = BTreeMap::new();
    for (key, value) in limits {
        let entry_table = value
            .as_table()
            .ok_or_else(|| type_error(source, &field("model_limits", key), "table"))?;
        result.insert(
            key.clone(),
            ModelLimitOverride::from_table(entry_table, source, &field("model_limits", key))?,
        );
    }
    Ok(Some(result))
}

fn merge_model_limit_maps(
    target: &mut Option<BTreeMap<String, ModelLimitOverride>>,
    next: Option<BTreeMap<String, ModelLimitOverride>>,
) {
    let Some(next) = next else {
        return;
    };
    let target = target.get_or_insert_with(BTreeMap::new);
    for (key, override_entry) in next {
        match target.entry(key) {
            Entry::Occupied(mut entry) => entry.get_mut().merge(override_entry),
            Entry::Vacant(entry) => {
                entry.insert(override_entry);
            }
        }
    }
}

fn merge_tui_theme_maps(
    target: &mut BTreeMap<String, TuiThemeSettings>,
    next: BTreeMap<String, TuiThemeSettings>,
) {
    for (name, theme) in next {
        match target.entry(name) {
            Entry::Occupied(mut entry) => entry.get_mut().merge(theme),
            Entry::Vacant(entry) => {
                entry.insert(theme);
            }
        }
    }
}

fn merge_profiles_maps(
    target: &mut Option<BTreeMap<String, SettingsFile>>,
    next: Option<BTreeMap<String, SettingsFile>>,
) {
    let Some(next) = next else {
        return;
    };
    let target = target.get_or_insert_with(BTreeMap::new);
    for (name, profile) in next {
        match target.entry(name) {
            Entry::Occupied(mut entry) => entry.get_mut().merge(profile),
            Entry::Vacant(entry) => {
                entry.insert(profile);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TurnId(u64);

impl TurnId {
    /// Sentinel value for events that originate outside any specific turn
    /// (e.g. manual `/compact` invoked between turns). Subscribers that
    /// key off `turn_id` can match on this to recognise out-of-turn events
    /// without conflating them with a real turn-0.
    pub const INVALID: Self = Self(u64::MAX);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "turn-{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningKind {
    Summary,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnthropicThinkingKind {
    Thinking,
    Redacted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnthropicThinkingBlock {
    pub kind: AnthropicThinkingKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ReasoningPayload {
    OpenAi {
        item_id: String,
        summary: Vec<String>,
        encrypted_content: Option<String>,
    },
    Anthropic {
        blocks: Vec<AnthropicThinkingBlock>,
    },
    Google {
        summary: Vec<String>,
        thought_signature: Option<String>,
    },
}

impl ReasoningPayload {
    pub fn provider_name(&self) -> &'static str {
        match self {
            ReasoningPayload::OpenAi { .. } => "openai",
            ReasoningPayload::Anthropic { .. } => "anthropic",
            ReasoningPayload::Google { .. } => "google",
        }
    }

    pub fn display_text(&self) -> String {
        match self {
            ReasoningPayload::OpenAi { summary, .. } => summary.join("\n\n"),
            ReasoningPayload::Anthropic { blocks } => {
                const REDACTED_REASONING: &str = "[redacted reasoning]";
                let capacity = blocks
                    .iter()
                    .map(|block| match block.kind {
                        AnthropicThinkingKind::Thinking => block.text.len(),
                        AnthropicThinkingKind::Redacted => REDACTED_REASONING.len(),
                    })
                    .sum::<usize>()
                    + blocks.len().saturating_sub(1) * 2;
                let mut text = String::with_capacity(capacity);
                for (index, block) in blocks.iter().enumerate() {
                    if index > 0 {
                        text.push_str("\n\n");
                    }
                    match block.kind {
                        AnthropicThinkingKind::Thinking => text.push_str(&block.text),
                        AnthropicThinkingKind::Redacted => text.push_str(REDACTED_REASONING),
                    }
                }
                text
            }
            ReasoningPayload::Google { summary, .. } => summary.join("\n\n"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningSnapshot {
    pub display_text: String,
    pub payload: ReasoningPayload,
}

impl ReasoningSnapshot {
    pub fn from_payload(payload: ReasoningPayload) -> Self {
        let display_text = payload.display_text();
        Self {
            display_text,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptItem {
    pub role: Role,
    pub content: String,
    /// Boxed to keep `TranscriptItem` small: it sits inside `AgentEvent`
    /// variants and the unboxed snapshot is large enough to trip clippy's
    /// `large_enum_variant` threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Box<ReasoningSnapshot>>,
    /// Set on assistant messages whose turn was cancelled mid-stream. The
    /// `content` is the partial text that was streamed before the user
    /// pressed Esc; downstream renderers append a `(cancelled)` marker so
    /// the next turn (and `/diff`/`/undo`) can reference the cut-off work.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cancelled: bool,
}

impl TranscriptItem {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            reasoning: None,
            cancelled: false,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: None,
            cancelled: false,
        }
    }

    pub fn assistant_with_reasoning(
        content: impl Into<String>,
        reasoning: Option<ReasoningSnapshot>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: reasoning.map(Box::new),
            cancelled: false,
        }
    }

    /// Builds an assistant transcript item from a turn that was cancelled
    /// mid-stream. The `content` is whatever streamed before the cancel —
    /// possibly empty if the model never started producing text.
    pub fn assistant_cancelled(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: None,
            cancelled: true,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            reasoning: None,
            cancelled: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAttachmentSource {
    Paste,
    File,
}

impl ContextAttachmentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paste => "paste",
            Self::File => "file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAttachmentKind {
    Log,
    StackTrace,
    Config,
    Text,
    UnsupportedBinary,
    /// Image-shaped payload (label extension or non-canonical magic
    /// bytes) that cannot be routed to a vision model — typically
    /// HEIC/BMP/TIFF or a `.png` label whose body is empty/garbled.
    /// The bytes are dropped and the attachment is marked
    /// [`ContextAttachmentStatus::Unsupported`].
    UnsupportedImage,
    /// Vision-routable image payload (PNG/JPEG/GIF/WEBP confirmed by
    /// magic bytes). The raw bytes are retained on
    /// [`ContextAttachment::image_data_base64`] so the agent can emit a
    /// matching `LlmInputItem::Image` per turn when the active model
    /// advertises vision capability.
    Image,
}

impl ContextAttachmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Log => "log",
            Self::StackTrace => "stack_trace",
            Self::Config => "config",
            Self::Text => "text",
            Self::UnsupportedBinary => "unsupported_binary",
            Self::UnsupportedImage => "unsupported_image",
            Self::Image => "image",
        }
    }

    pub fn is_supported_text(self) -> bool {
        !matches!(
            self,
            Self::UnsupportedBinary | Self::UnsupportedImage | Self::Image
        )
    }

    /// `true` for the vision-routable [`Self::Image`] kind. Used at
    /// request-build time to fan a single attachment out into a
    /// `LlmInputItem::Image` so the bytes reach the provider verbatim
    /// instead of being squashed into the user text reference block.
    pub fn is_routable_image(self) -> bool {
        matches!(self, Self::Image)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAttachmentStatus {
    Attached,
    Removed,
    Unsupported,
}

impl ContextAttachmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attached => "attached",
            Self::Removed => "removed",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAttachment {
    pub id: String,
    pub source: ContextAttachmentSource,
    pub kind: ContextAttachmentKind,
    pub status: ContextAttachmentStatus,
    pub label: String,
    pub path: Option<String>,
    pub original_sha256: String,
    pub redacted_sha256: Option<String>,
    pub original_bytes: usize,
    pub stored_bytes: usize,
    pub preview_bytes: usize,
    pub redactions: u64,
    pub preview: String,
    pub truncated: bool,
    /// MIME type for vision-routable image attachments
    /// (`image/{png,jpeg,gif,webp}`). `None` for text/log/binary kinds
    /// and for label-only `UnsupportedImage` payloads whose magic
    /// bytes did not match a canonical format.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_media_type: Option<String>,
    /// Base64-encoded original image bytes for vision-routable image
    /// attachments. Stored alongside the JSON checkpoint so resume
    /// rehydrates an `LlmInputItem::Image` without re-reading the
    /// source. `None` outside the [`ContextAttachmentKind::Image`]
    /// kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_data_base64: Option<String>,
}

impl ContextAttachment {
    pub fn is_active(&self) -> bool {
        self.status == ContextAttachmentStatus::Attached
    }

    pub fn reference(&self) -> String {
        format!("attachment://{}", self.id)
    }
}

pub fn detect_context_attachment_kind(
    label: Option<&str>,
    bytes: &[u8],
    text: Option<&str>,
) -> ContextAttachmentKind {
    // Magic-byte-detected images route to the vision pipeline; the
    // squeezy-llm providers re-encode the bytes inline for the
    // provider's native wire format. Label-only "image-looking"
    // payloads (extension matched but magic bytes didn't) stay
    // `UnsupportedImage` so we don't ship garbled bytes to a model
    // that can't decode them.
    if detect_image_mime(bytes).is_some() {
        return ContextAttachmentKind::Image;
    }
    if looks_like_image(label, bytes) {
        return ContextAttachmentKind::UnsupportedImage;
    }
    let Some(text) = text else {
        return ContextAttachmentKind::UnsupportedBinary;
    };
    if looks_like_binary(bytes) {
        return ContextAttachmentKind::UnsupportedBinary;
    }
    if looks_like_stack_trace(text) {
        return ContextAttachmentKind::StackTrace;
    }
    if looks_like_log(text) {
        return ContextAttachmentKind::Log;
    }
    if looks_like_config(label, text) {
        return ContextAttachmentKind::Config;
    }
    ContextAttachmentKind::Text
}

/// Detect the canonical image MIME type from a byte prefix using
/// magic numbers. Supports PNG, JPEG, GIF (87a/89a), and WEBP
/// (RIFF / WEBP container) — the set of formats the upstream vision
/// providers (Anthropic / OpenAI / Google / Bedrock / Bedrock
/// Claude) all accept for inline image content blocks. Returns
/// `None` when the prefix does not match a known image format so the
/// caller can fall back to label-only detection or to the
/// `UnsupportedImage` path.
///
/// Mirrors [`squeezy_llm::infer_image_mime`] but lives in
/// `squeezy-core` so the attachment detection layer (which is
/// upstream of `squeezy-llm`) can short-circuit on magic bytes
/// without depending on the LLM crate.
pub fn detect_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

pub fn context_attachment_preview(text: &str, max_bytes: usize) -> (String, bool) {
    truncate_utf8(text, max_bytes)
}

pub fn context_attachment_storage_text(text: &str, max_bytes: usize) -> (String, bool) {
    truncate_utf8(text, max_bytes)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEstimate {
    pub bytes: usize,
    pub estimated_tokens: u64,
    pub items: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextCompactionTrigger {
    #[default]
    Auto,
    Manual,
}

impl ContextCompactionTrigger {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextPin {
    pub id: String,
    pub label: String,
    pub summary: String,
    pub source: String,
    pub created_unix_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionRecord {
    pub generation: u64,
    pub trigger: ContextCompactionTrigger,
    pub compacted_at_ms: u64,
    pub before: ContextEstimate,
    pub after: ContextEstimate,
    pub dropped_items: usize,
    pub summary_bytes: usize,
    /// Stable id of the pre-compaction snapshot persisted in
    /// `compaction_checkpoints`. Populated when the agent had a `SqueezyStore`
    /// handle at compaction time; `None` for sessions without persistence or
    /// when the checkpoint write itself failed (non-fatal).
    #[serde(default)]
    pub replacement_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionState {
    pub generation: u64,
    pub summary: Option<String>,
    pub pinned: Vec<ContextPin>,
    pub last: Option<ContextCompactionRecord>,
    #[serde(default)]
    pub history: Vec<ContextCompactionRecord>,
}

fn truncate_utf8(text: &str, max_bytes: usize) -> (String, bool) {
    if max_bytes == 0 {
        return (String::new(), !text.is_empty());
    }
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn looks_like_image(label: Option<&str>, bytes: &[u8]) -> bool {
    let lower_label = label.unwrap_or_default().to_ascii_lowercase();
    if matches!(
        lower_label.rsplit('.').next(),
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "heic")
    ) {
        return true;
    }
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP")
}

fn looks_like_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let sample = &bytes[..bytes.len().min(4096)];
    if sample.contains(&0) {
        return true;
    }
    let control = sample
        .iter()
        .filter(|byte| {
            let byte = **byte;
            byte < 0x09 || (byte > 0x0d && byte < 0x20)
        })
        .count();
    control.saturating_mul(100) > sample.len().saturating_mul(10)
}

fn looks_like_stack_trace(text: &str) -> bool {
    if ascii_contains_ignore_case(text, "traceback (most recent call last)")
        || ascii_contains_ignore_case(text, "stack backtrace:")
        || ascii_contains_ignore_case(text, "caused by:")
        || ascii_contains_ignore_case(text, "thread '")
        || ascii_contains_ignore_case(text, "panic")
        || ascii_contains_ignore_case(text, "exception in thread")
    {
        return true;
    }
    let stackish_lines = text
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("at ")
                || trimmed.starts_with("File \"")
                || trimmed.starts_with("from ")
                || trimmed.starts_with("error[E")
                || trimmed.starts_with("#")
        })
        .take(3)
        .count();
    stackish_lines >= 2
}

fn ascii_contains_ignore_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn looks_like_log(text: &str) -> bool {
    let mut logish = 0usize;
    let mut lines = 0usize;
    for line in text.lines().take(20) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        lines += 1;
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("error")
            || lower.starts_with("warn")
            || lower.starts_with("info")
            || lower.starts_with("debug")
            || lower.starts_with("trace")
            || lower.contains(" error ")
            || lower.contains(" warn ")
            || lower.contains(" failed")
            || starts_with_timestamp(trimmed)
        {
            logish += 1;
        }
    }
    lines >= 2 && logish >= 2
}

fn starts_with_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() >= 10
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
    {
        return true;
    }
    bytes.len() >= 8
        && bytes[0..2].iter().all(u8::is_ascii_digit)
        && bytes[2] == b':'
        && bytes[3..5].iter().all(u8::is_ascii_digit)
        && bytes[5] == b':'
        && bytes[6..8].iter().all(u8::is_ascii_digit)
}

fn looks_like_config(label: Option<&str>, text: &str) -> bool {
    let lower_label = label.unwrap_or_default().to_ascii_lowercase();
    if matches!(
        lower_label.rsplit('.').next(),
        Some(
            "toml"
                | "yaml"
                | "yml"
                | "json"
                | "jsonl"
                | "env"
                | "ini"
                | "properties"
                | "conf"
                | "config"
        )
    ) {
        return true;
    }
    let mut configish = 0usize;
    let mut lines = 0usize;
    for line in text.lines().take(20) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        lines += 1;
        if trimmed.starts_with('{')
            || trimmed.starts_with('[')
            || trimmed.contains('=')
            || trimmed.contains(": ")
        {
            configish += 1;
        }
    }
    lines > 0 && configish.saturating_mul(100) >= lines.saturating_mul(60)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostSnapshot {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Reasoning portion of `output_tokens`. By cross-provider
    /// convention this is a *subset* of `output_tokens` (the inclusive
    /// generated-token total), mirroring `cached_input_tokens` as a
    /// subset of `input_tokens` — it is a breakdown, not an addend, so
    /// total-token math must not add it on top of `output_tokens`.
    #[serde(default)]
    pub reasoning_output_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub estimated_usd_micros: Option<u64>,
}

/// Per-`SubagentKind` rollup attached to [`TurnMetrics`] /
/// [`SessionMetrics`]. Each bucket carries the same counters as the
/// aggregate `subagent_*` fields but scoped to a single kind so the
/// operator can answer "is Explore burning the bulk of subagent tokens?"
/// and tune `explore_model` accordingly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentKindMetrics {
    #[serde(default)]
    pub delegate: SubagentKindBucket,
    #[serde(default)]
    pub explore: SubagentKindBucket,
    #[serde(default)]
    pub plan: SubagentKindBucket,
    #[serde(default)]
    pub review: SubagentKindBucket,
}

impl SubagentKindMetrics {
    pub fn merge(&mut self, other: &SubagentKindMetrics) {
        self.delegate.merge(&other.delegate);
        self.explore.merge(&other.explore);
        self.plan.merge(&other.plan);
        self.review.merge(&other.review);
    }

    /// Mutable handle to the bucket for `kind`. Returns `None` for kinds
    /// outside the four audited buckets (delegate/explore/plan/review)
    /// so callers can ignore intra-agent helper kinds like `doc_help`
    /// without polluting the rollup.
    pub fn bucket_mut(&mut self, kind: &str) -> Option<&mut SubagentKindBucket> {
        match kind {
            "delegate" => Some(&mut self.delegate),
            "explore" => Some(&mut self.explore),
            "plan" => Some(&mut self.plan),
            "review" => Some(&mut self.review),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentKindBucket {
    pub calls: u64,
    pub failures: u64,
    pub tool_calls: u64,
    pub bytes_read: u64,
    pub provider: CostSnapshot,
}

impl SubagentKindBucket {
    pub fn merge(&mut self, other: &SubagentKindBucket) {
        self.calls += other.calls;
        self.failures += other.failures;
        self.tool_calls += other.tool_calls;
        self.bytes_read += other.bytes_read;
        merge_cost_snapshot(&mut self.provider, &other.provider);
    }
}

/// Origin of a recorded provider cost: the main agent's own LLM rounds vs.
/// work done inside a dispatched subagent. Selects which slot of a
/// [`ModelCostBucket`] a round folds into. Not serialized — it is a
/// record-time selector, not stored state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostOrigin {
    Main,
    Subagent,
}

/// Per-`(provider, model)` cost bucket, split by [`CostOrigin`] so `/cost`
/// can show what the main agent spent on a model separately from what
/// subagents spent on it. Each slot carries a full [`CostSnapshot`], so the
/// input/output/cache-read/cache-write distribution is preserved per model
/// and per origin. `provider`/`model` are stored inline because the map key
/// in [`ModelLedger`] must be an opaque string — `serde_json` cannot
/// serialize a map keyed by a struct.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCostBucket {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub main: CostSnapshot,
    #[serde(default)]
    pub subagent: CostSnapshot,
}

impl ModelCostBucket {
    pub fn merge(&mut self, other: &ModelCostBucket) {
        merge_cost_snapshot(&mut self.main, &other.main);
        merge_cost_snapshot(&mut self.subagent, &other.subagent);
    }

    /// Combined estimated USD across both origins (`None` only when neither
    /// slot carries a priced round).
    pub fn total_usd_micros(&self) -> Option<u64> {
        add_optional_u64(
            self.main.estimated_usd_micros,
            self.subagent.estimated_usd_micros,
        )
    }
}

/// Per-`(provider, model)` cost ledger attached to [`TurnMetrics`] /
/// [`SessionMetrics`]. Additive-only and parallel to the flat
/// `provider`/`subagent_provider` totals — it is never summed into the
/// session dollar total, only used to attribute already-computed spend to the
/// model that produced it. Keyed by an opaque `provider\u{1f}model` string
/// (the unit separator can't appear in a provider or model id), so the value
/// round-trips through `serde_json`; render from the bucket's stored
/// `provider`/`model` rather than parsing the key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelLedger(#[serde(default)] pub std::collections::BTreeMap<String, ModelCostBucket>);

impl ModelLedger {
    fn key(provider: &str, model: &str) -> String {
        format!("{provider}\u{1f}{model}")
    }

    /// Fold one round's cost into the `(provider, model)` bucket on the side
    /// selected by `origin`. Reuses [`merge_cost_snapshot`] so the field-wise
    /// accumulation matches every other cost rollup.
    pub fn record(&mut self, provider: &str, model: &str, origin: CostOrigin, cost: &CostSnapshot) {
        let bucket = self
            .0
            .entry(Self::key(provider, model))
            .or_insert_with(|| ModelCostBucket {
                provider: provider.to_string(),
                model: model.to_string(),
                ..Default::default()
            });
        let slot = match origin {
            CostOrigin::Main => &mut bucket.main,
            CostOrigin::Subagent => &mut bucket.subagent,
        };
        merge_cost_snapshot(slot, cost);
    }

    pub fn merge(&mut self, other: &ModelLedger) {
        for (key, bucket) in other.0.iter() {
            self.0
                .entry(key.clone())
                .or_insert_with(|| ModelCostBucket {
                    provider: bucket.provider.clone(),
                    model: bucket.model.clone(),
                    ..Default::default()
                })
                .merge(bucket);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ModelCostBucket> {
        self.0.values()
    }

    /// Combined cost across every bucket and both origins (main + subagent).
    /// Used for the `/cost` "By model" Σ total row; equals the session's
    /// `provider` + `subagent_provider` aggregate by construction, so the
    /// drill's total matches the headline.
    pub fn totals(&self) -> CostSnapshot {
        let mut total = CostSnapshot::default();
        for bucket in self.0.values() {
            merge_cost_snapshot(&mut total, &bucket.main);
            merge_cost_snapshot(&mut total, &bucket.subagent);
        }
        total
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub turns: u64,
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_errors: u64,
    pub tool_denials: u64,
    pub tool_cancellations: u64,
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub matches_returned: u64,
    pub model_output_bytes: u64,
    pub receipt_stub_hits: u64,
    pub negative_receipt_hits: u64,
    pub spill_writes: u64,
    pub spill_reads: u64,
    pub budget_denials: u64,
    pub planner_turns: u64,
    pub planner_tool_calls: u64,
    pub planner_refusals: u64,
    pub subagent_calls: u64,
    pub subagent_failures: u64,
    pub subagent_tool_calls: u64,
    pub subagent_budget_denials: u64,
    pub subagent_files_scanned: u64,
    pub subagent_bytes_read: u64,
    pub subagent_model_output_bytes: u64,
    pub redactions: u64,
    pub provider: CostSnapshot,
    pub subagent_provider: CostSnapshot,
    #[serde(default)]
    pub subagent_by_kind: SubagentKindMetrics,
    /// Per-`(provider, model)` cost ledger: attributes the session's
    /// already-computed spend to the model that produced it, split main vs
    /// subagent. Additive-only and never summed into the dollar total. Empty
    /// on sessions persisted before this field existed.
    #[serde(default)]
    pub model_ledger: ModelLedger,
    /// Cumulative USD micros spent on cheap-tier routing-judge calls
    /// across the session.
    #[serde(default)]
    pub routing_judge_usd_micros: u64,
    /// Number of turns dispatched to the cheap tier instead of the
    /// parent model.
    #[serde(default)]
    pub routed_to_cheap_turns: u64,
    /// Number of cheap-routed turns that escalated back to the parent
    /// model mid-turn.
    #[serde(default)]
    pub escalated_to_parent_turns: u64,
    /// Cumulative estimated savings versus running the same turns on
    /// the parent model.
    #[serde(default)]
    pub routing_estimated_savings_usd_micros: u64,
    /// Cumulative net routing savings. Unlike
    /// `routing_estimated_savings_usd_micros`, this signed value can
    /// show tiny net-negative routed turns.
    #[serde(default)]
    pub routing_estimated_net_savings_usd_micros: i64,
}

impl SessionMetrics {
    pub fn merge_turn(&mut self, turn: &TurnMetrics) {
        self.turns += 1;
        self.tool_calls += turn.tool_calls;
        self.tool_successes += turn.tool_successes;
        self.tool_errors += turn.tool_errors;
        self.tool_denials += turn.tool_denials;
        self.tool_cancellations += turn.tool_cancellations;
        self.files_scanned += turn.files_scanned;
        self.bytes_read += turn.bytes_read;
        self.matches_returned += turn.matches_returned;
        self.model_output_bytes += turn.model_output_bytes;
        self.receipt_stub_hits += turn.receipt_stub_hits;
        self.negative_receipt_hits += turn.negative_receipt_hits;
        self.spill_writes += turn.spill_writes;
        self.spill_reads += turn.spill_reads;
        self.budget_denials += turn.budget_denials;
        self.planner_turns += turn.planner_turns;
        self.planner_tool_calls += turn.planner_tool_calls;
        self.planner_refusals += turn.planner_refusals;
        self.subagent_calls += turn.subagent_calls;
        self.subagent_failures += turn.subagent_failures;
        self.subagent_tool_calls += turn.subagent_tool_calls;
        self.subagent_budget_denials += turn.subagent_budget_denials;
        self.subagent_files_scanned += turn.subagent_files_scanned;
        self.subagent_bytes_read += turn.subagent_bytes_read;
        self.subagent_model_output_bytes += turn.subagent_model_output_bytes;
        self.redactions += turn.redactions;
        merge_cost_snapshot(&mut self.provider, &turn.provider);
        merge_cost_snapshot(&mut self.subagent_provider, &turn.subagent_provider);
        self.subagent_by_kind.merge(&turn.subagent_by_kind);
        self.model_ledger.merge(&turn.model_ledger);
        self.routing_judge_usd_micros = self
            .routing_judge_usd_micros
            .saturating_add(turn.routing_judge_usd_micros);
        if turn.routed_to_cheap {
            self.routed_to_cheap_turns = self.routed_to_cheap_turns.saturating_add(1);
        }
        if turn.escalated_to_parent {
            self.escalated_to_parent_turns = self.escalated_to_parent_turns.saturating_add(1);
        }
        self.routing_estimated_savings_usd_micros = self
            .routing_estimated_savings_usd_micros
            .saturating_add(turn.routing_estimated_savings_usd_micros);
        self.routing_estimated_net_savings_usd_micros = self
            .routing_estimated_net_savings_usd_micros
            .saturating_add(turn.routing_estimated_net_savings_usd_micros);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMetrics {
    pub tool_calls: u64,
    pub tool_successes: u64,
    pub tool_errors: u64,
    pub tool_denials: u64,
    pub tool_cancellations: u64,
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub matches_returned: u64,
    pub model_output_bytes: u64,
    pub receipt_stub_hits: u64,
    pub negative_receipt_hits: u64,
    pub spill_writes: u64,
    pub spill_reads: u64,
    pub budget_denials: u64,
    pub planner_turns: u64,
    pub planner_tool_calls: u64,
    pub planner_refusals: u64,
    pub subagent_calls: u64,
    pub subagent_failures: u64,
    pub subagent_tool_calls: u64,
    pub subagent_budget_denials: u64,
    pub subagent_files_scanned: u64,
    pub subagent_bytes_read: u64,
    pub subagent_model_output_bytes: u64,
    pub redactions: u64,
    pub provider: CostSnapshot,
    pub subagent_provider: CostSnapshot,
    #[serde(default)]
    pub subagent_by_kind: SubagentKindMetrics,
    /// Per-`(provider, model)` cost ledger for this turn, split main vs
    /// subagent. Folded into the session ledger by [`SessionMetrics::merge_turn`].
    #[serde(default)]
    pub model_ledger: ModelLedger,
    /// USD micros spent on the borderline-classification call
    /// dispatched by the cheap-model fast path. Zero on turns where the
    /// heuristic fired (no judge call) or routing was disabled.
    #[serde(default)]
    pub routing_judge_usd_micros: u64,
    /// Provider usage from the cheap-tier main turn only. Excludes the
    /// routing judge and any parent-model work after escalation so
    /// routing savings can be estimated from the tokens that actually
    /// benefited from cheap dispatch.
    #[serde(default)]
    pub routing_cheap_main_provider: CostSnapshot,
    /// True when the turn's first LLM round dispatched on the cheap
    /// tier rather than the user's configured parent model.
    #[serde(default)]
    pub routed_to_cheap: bool,
    /// True when a cheap-routed turn ran into an escalation signal and
    /// switched back to the parent model mid-turn.
    #[serde(default)]
    pub escalated_to_parent: bool,
    /// Estimated savings versus running the same turn on the parent
    /// model — computed by re-pricing the provider-reported token
    /// counts at the parent's per-Mtok rate and subtracting the actual
    /// cheap-tier bill. Zero when the turn was not routed or when the
    /// model registry has no pricing for either side.
    #[serde(default)]
    pub routing_estimated_savings_usd_micros: u64,
    /// Signed net routing savings for this turn. Positive means the
    /// cheap path saved money versus the parent estimate; negative
    /// means judge/cheap overhead exceeded the estimated parent cost.
    #[serde(default)]
    pub routing_estimated_net_savings_usd_micros: i64,
    /// Normalized stop reason token for the most recent LLM completion in this
    /// turn (e.g. `"end_turn"`, `"max_tokens"`, `"refusal"`). `None` when the
    /// turn did not complete normally or no event was received.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason_token: Option<String>,
    /// True when the provider's completion carried `reasoning_only_stop`: the
    /// model spent the round on hidden reasoning and produced no visible output.
    #[serde(default)]
    pub reasoning_only_stop: bool,
    /// Whether prompt caching was supported for this turn (true if either
    /// `cached_input_tokens` or `cache_write_input_tokens` was non-zero).
    #[serde(default)]
    pub cache_supported: bool,
    /// Cache-write (creation) tokens for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
    /// Reasoning output tokens for this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output_tokens: Option<u64>,
}

impl TurnMetrics {
    pub fn record_provider(&mut self, cost: &CostSnapshot) {
        merge_cost_snapshot(&mut self.provider, cost);
    }

    /// Roll up the subagent's own [`TurnMetrics`] into the parent turn. The
    /// subagent's tool / I/O / provider counters are attributed to
    /// `subagent_*` so the parent's tool / I/O / provider numbers stay scoped
    /// to the parent agent's own work, while `redactions` is a session-wide
    /// safety counter and is merged into the parent total instead of dropped.
    pub fn merge_subagent_tool_metrics(&mut self, metrics: &TurnMetrics) {
        self.subagent_tool_calls += metrics.tool_calls;
        self.subagent_budget_denials += metrics.budget_denials;
        self.subagent_files_scanned += metrics.files_scanned;
        self.subagent_bytes_read += metrics.bytes_read;
        self.subagent_model_output_bytes += metrics.model_output_bytes;
        self.redactions += metrics.redactions;
        merge_cost_snapshot(&mut self.subagent_provider, &metrics.provider);
    }
}

fn merge_cost_snapshot(total: &mut CostSnapshot, next: &CostSnapshot) {
    total.input_tokens = add_optional_u64(total.input_tokens, next.input_tokens);
    total.output_tokens = add_optional_u64(total.output_tokens, next.output_tokens);
    total.reasoning_output_tokens =
        add_optional_u64(total.reasoning_output_tokens, next.reasoning_output_tokens);
    total.cached_input_tokens =
        add_optional_u64(total.cached_input_tokens, next.cached_input_tokens);
    total.cache_write_input_tokens = add_optional_u64(
        total.cache_write_input_tokens,
        next.cache_write_input_tokens,
    );
    total.estimated_usd_micros =
        add_optional_u64(total.estimated_usd_micros, next.estimated_usd_micros);
}

fn add_optional_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    [left, right].into_iter().flatten().reduce(|a, b| a + b)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash(pub String);

impl ContentHash {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileId(pub String);

impl FileId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SymbolId(pub String);

impl SymbolId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourcePoint {
    pub line: u32,
    pub column: u32,
}

impl SourcePoint {
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceSpan {
    pub start_byte: u32,
    pub end_byte: u32,
    pub start: SourcePoint,
    pub end: SourcePoint,
}

impl SourceSpan {
    pub const fn new(start_byte: u32, end_byte: u32, start: SourcePoint, end: SourcePoint) -> Self {
        Self {
            start_byte,
            end_byte,
            start,
            end,
        }
    }

    /// Inclusive point-membership test for a single byte *position*.
    ///
    /// This intentionally treats `end_byte` as a valid position: callers that
    /// probe an *exclusive* half-open boundary (e.g. another span's
    /// `end_byte`) rely on it so that a child span exactly filling its parent
    /// — `child.end_byte == parent.end_byte` — still reads as inside. For
    /// span-vs-span containment use [`Self::contains_span`], which applies the
    /// correct half-open semantics in one place rather than two byte probes.
    pub const fn contains_byte(self, byte: u32) -> bool {
        self.start_byte <= byte && byte <= self.end_byte
    }

    /// Half-open `[start, end)` span containment: true when `other` lies fully
    /// inside `self`. A zero-width touch at the boundary is NOT containment —
    /// a span starting exactly at `self.end_byte` (or an empty span there) is
    /// excluded, because the parent's last addressed byte is `end_byte - 1`.
    /// The single exception is a zero-width `other` sitting at `self.start`,
    /// which is still inside.
    pub const fn contains_span(self, other: Self) -> bool {
        // Empty parents address no bytes, so they contain nothing.
        self.start_byte < self.end_byte
            && self.start_byte <= other.start_byte
            && other.end_byte <= self.end_byte
            // Reject a child that begins on the exclusive boundary: a span
            // starting at `self.end_byte` only "fits" because `end <= end`,
            // but it touches the boundary rather than living inside it.
            && other.start_byte < self.end_byte
    }

    /// Half-open `[start, end)` overlap test: true when the two spans share at
    /// least one addressed byte. A boundary touch — one span ending exactly
    /// where the other starts (`a.end_byte == b.start_byte`) — is NOT an
    /// overlap, and an empty span (which addresses no bytes) never overlaps
    /// anything.
    pub const fn overlaps(self, other: Self) -> bool {
        // Empty spans address no bytes; the half-open intersection test below
        // would otherwise report a zero-width span sitting inside the other as
        // an overlap.
        self.start_byte < self.end_byte
            && other.start_byte < other.end_byte
            && self.start_byte < other.end_byte
            && other.start_byte < self.end_byte
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageKind {
    C,
    CSharp,
    Cpp,
    Dart,
    Go,
    Java,
    JavaScript,
    Jsx,
    Kotlin,
    Php,
    Python,
    Ruby,
    Rust,
    Scala,
    Swift,
    TypeScript,
    Tsx,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageFamily {
    Rust,
    Python,
    Java,
    CSharp,
    Go,
    CFamily,
    JsTs,
    Ruby,
    Php,
    Kotlin,
    Swift,
    Scala,
    Dart,
}

impl LanguageFamily {
    pub const ALL: [Self; 13] = [
        Self::Rust,
        Self::Python,
        Self::Java,
        Self::CSharp,
        Self::Go,
        Self::CFamily,
        Self::JsTs,
        Self::Ruby,
        Self::Php,
        Self::Kotlin,
        Self::Swift,
        Self::Scala,
        Self::Dart,
    ];

    pub const fn all() -> &'static [Self] {
        &Self::ALL
    }

    pub const fn id(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Java => "java",
            Self::CSharp => "csharp",
            Self::Go => "go",
            Self::CFamily => "c-family",
            Self::JsTs => "js-ts",
            Self::Ruby => "ruby",
            Self::Php => "php",
            Self::Kotlin => "kotlin",
            Self::Swift => "swift",
            Self::Scala => "scala",
            Self::Dart => "dart",
        }
    }

    /// Human-readable label suitable for prose (tool descriptions, docs).
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::Java => "Java",
            Self::CSharp => "C#",
            Self::Go => "Go",
            Self::CFamily => "C/C++",
            Self::JsTs => "JavaScript/TypeScript",
            Self::Ruby => "Ruby",
            Self::Php => "PHP",
            Self::Kotlin => "Kotlin",
            Self::Swift => "Swift",
            Self::Scala => "Scala",
            Self::Dart => "Dart",
        }
    }

    pub const fn of(kind: LanguageKind) -> Option<Self> {
        match kind {
            LanguageKind::Rust => Some(Self::Rust),
            LanguageKind::Python => Some(Self::Python),
            LanguageKind::Java => Some(Self::Java),
            LanguageKind::CSharp => Some(Self::CSharp),
            LanguageKind::Go => Some(Self::Go),
            LanguageKind::C | LanguageKind::Cpp => Some(Self::CFamily),
            LanguageKind::JavaScript
            | LanguageKind::Jsx
            | LanguageKind::TypeScript
            | LanguageKind::Tsx => Some(Self::JsTs),
            LanguageKind::Ruby => Some(Self::Ruby),
            LanguageKind::Php => Some(Self::Php),
            LanguageKind::Kotlin => Some(Self::Kotlin),
            LanguageKind::Swift => Some(Self::Swift),
            LanguageKind::Scala => Some(Self::Scala),
            LanguageKind::Dart => Some(Self::Dart),
            LanguageKind::Unsupported | LanguageKind::Unknown => None,
        }
    }

    pub const fn kinds(self) -> &'static [LanguageKind] {
        match self {
            Self::Rust => &[LanguageKind::Rust],
            Self::Python => &[LanguageKind::Python],
            Self::Java => &[LanguageKind::Java],
            Self::CSharp => &[LanguageKind::CSharp],
            Self::Go => &[LanguageKind::Go],
            Self::CFamily => &[LanguageKind::C, LanguageKind::Cpp],
            Self::JsTs => &[
                LanguageKind::JavaScript,
                LanguageKind::Jsx,
                LanguageKind::TypeScript,
                LanguageKind::Tsx,
            ],
            Self::Ruby => &[LanguageKind::Ruby],
            Self::Php => &[LanguageKind::Php],
            Self::Kotlin => &[LanguageKind::Kotlin],
            Self::Swift => &[LanguageKind::Swift],
            Self::Scala => &[LanguageKind::Scala],
            Self::Dart => &[LanguageKind::Dart],
        }
    }

    pub const fn file_extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::Python => &["py"],
            Self::Java => &["java"],
            Self::CSharp => &["cs", "csx"],
            Self::Go => &["go"],
            Self::CFamily => &["c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx"],
            Self::JsTs => &["cjs", "cts", "js", "jsx", "mjs", "mts", "ts", "tsx"],
            Self::Ruby => &["rb"],
            Self::Php => &["php"],
            Self::Kotlin => &["kt", "kts"],
            Self::Swift => &["swift"],
            Self::Scala => &["scala", "sc"],
            Self::Dart => &["dart"],
        }
    }
}

impl LanguageKind {
    pub const fn family(self) -> Option<LanguageFamily> {
        LanguageFamily::of(self)
    }

    pub const fn from_extension(extension: &str) -> Self {
        match extension.as_bytes() {
            b"c" => Self::C,
            b"cc" | b"cpp" | b"cxx" | b"hh" | b"hpp" | b"hxx" => Self::Cpp,
            b"h" => Self::Cpp,
            b"cs" | b"csx" => Self::CSharp,
            b"cjs" | b"js" | b"mjs" => Self::JavaScript,
            b"cts" | b"mts" | b"ts" => Self::TypeScript,
            b"dart" => Self::Dart,
            b"go" => Self::Go,
            b"java" => Self::Java,
            b"jsx" => Self::Jsx,
            b"kt" | b"kts" => Self::Kotlin,
            b"php" => Self::Php,
            b"py" => Self::Python,
            b"rb" => Self::Ruby,
            b"rs" => Self::Rust,
            b"scala" | b"sc" => Self::Scala,
            b"swift" => Self::Swift,
            b"tsx" => Self::Tsx,
            _ => Self::Unsupported,
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::C => "C",
            Self::CSharp => "C#",
            Self::Cpp => "C++",
            Self::Dart => "Dart",
            Self::Go => "Go",
            Self::Java => "Java",
            Self::JavaScript => "JavaScript",
            Self::Jsx => "JSX",
            Self::Kotlin => "Kotlin",
            Self::Php => "PHP",
            Self::Python => "Python",
            Self::Ruby => "Ruby",
            Self::Rust => "Rust",
            Self::Scala => "Scala",
            Self::Swift => "Swift",
            Self::TypeScript => "TypeScript",
            Self::Tsx => "TSX",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OracleId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Class,
    Crate,
    File,
    Interface,
    Module,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Function,
    Method,
    Const,
    Static,
    TypeAlias,
    Field,
    Variant,
    Macro,
    Test,
    Unknown,
}

/// Stable kind tag attached to every `GraphEdge`. New variants append at the
/// end so serialized graphs stay forward-compatible: callers that ignore
/// unknown kinds keep working when a deployment lands a kind they haven't
/// seen yet. Inheritance-style kinds (`Extends`, `Implements`, `UsesTrait`)
/// participate in ancestor walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EdgeKind {
    Contains,
    Imports,
    Reexports,
    Calls,
    References,
    Extends,
    PartialOf,
    Implements,
    InherentImpl,
    TraitImpl,
    TestOf,
    DefinesMacro,
    InvokesMacro,
    Conditional,
    /// PHP trait inclusion (`use TraitA;` inside a class/trait body). Modelled
    /// alongside `Extends`/`Implements` so ancestor-style queries can walk a
    /// class up through its included traits the same way they walk parents
    /// and implemented interfaces.
    UsesTrait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Confidence {
    ExactSyntax,
    ImportResolved,
    Heuristic,
    CandidateSet,
    External,
    MacroOpaque,
    ConditionalUnknown,
    Unsupported,
    Stale,
    Partial,
}

impl Confidence {
    /// Every variant in declaration order. Use this for iteration when
    /// building distributions or summarising packets.
    pub const ALL: [Self; 10] = [
        Self::ExactSyntax,
        Self::ImportResolved,
        Self::Heuristic,
        Self::CandidateSet,
        Self::External,
        Self::MacroOpaque,
        Self::ConditionalUnknown,
        Self::Unsupported,
        Self::Stale,
        Self::Partial,
    ];

    /// Stable snake_case identifier suitable for JSON map keys.
    pub const fn id(self) -> &'static str {
        match self {
            Self::ExactSyntax => "exact_syntax",
            Self::ImportResolved => "import_resolved",
            Self::Heuristic => "heuristic",
            Self::CandidateSet => "candidate_set",
            Self::External => "external",
            Self::MacroOpaque => "macro_opaque",
            Self::ConditionalUnknown => "conditional_unknown",
            Self::Unsupported => "unsupported",
            Self::Stale => "stale",
            Self::Partial => "partial",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Freshness {
    Fresh,
    Stale,
    Partial,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    pub reason: String,
}

impl Provenance {
    pub fn new(source: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Error)]
pub enum SqueezyError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("provider is not configured: {0}")]
    ProviderNotConfigured(String),
    #[error("provider request failed: {0}")]
    ProviderRequest(String),
    #[error("provider stream failed: {0}")]
    ProviderStream(String),
    #[error("terminal error: {0}")]
    Terminal(String),
    #[error("agent error: {0}")]
    Agent(String),
    #[error("workspace error: {0}")]
    Workspace(String),
    #[error("parse error: {0}")]
    Parse(String),
    #[error("graph error: {0}")]
    Graph(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("permission denied: {0}")]
    Permission(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SqueezyError>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStateStatus {
    #[default]
    Running,
    Blocked,
    Completed,
    Cancelled,
    Failed,
}

impl TaskStateStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStepStatus {
    #[default]
    Pending,
    Active,
    Completed,
    Blocked,
    Skipped,
}

impl TaskStepStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskVerificationState {
    #[default]
    NotStarted,
    Running,
    Passed,
    Failed,
    Skipped,
}

impl TaskVerificationState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Running => "running",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStateStep {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: TaskStepStatus,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStateSnapshot {
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub status: TaskStateStatus,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub steps: Vec<TaskStateStep>,
    #[serde(default)]
    pub blocker: Option<String>,
    #[serde(default)]
    pub next_action: Option<String>,
    #[serde(default)]
    pub verification: TaskVerificationState,
    #[serde(default)]
    pub recent_changes: Vec<String>,
    #[serde(default)]
    pub replan_reason: Option<String>,
}

impl TaskStateSnapshot {
    pub fn starting(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            status: TaskStateStatus::Running,
            steps: vec![TaskStateStep {
                title: "Start turn".to_string(),
                status: TaskStepStatus::Active,
                detail: Some("Preparing the first model request".to_string()),
            }],
            next_action: Some("wait for agent task-state update".to_string()),
            ..Self::default()
        }
        .normalized()
    }

    pub fn terminal_from(
        latest: Option<&Self>,
        fallback_task: impl Into<String>,
        status: TaskStateStatus,
        summary: Option<String>,
    ) -> Self {
        let mut snapshot = latest
            .cloned()
            .unwrap_or_else(|| Self::starting(fallback_task));
        snapshot.status = status;
        snapshot.summary = summary.or(snapshot.summary);
        if matches!(
            status,
            TaskStateStatus::Completed | TaskStateStatus::Cancelled | TaskStateStatus::Failed
        ) {
            snapshot.next_action = None;
        }
        snapshot.normalized()
    }

    pub fn active_step_title(&self) -> Option<&str> {
        self.steps
            .iter()
            .find(|step| {
                matches!(
                    step.status,
                    TaskStepStatus::Active | TaskStepStatus::Blocked
                )
            })
            .map(|step| step.title.as_str())
    }

    pub fn compact_summary(&self) -> String {
        let mut summary = String::with_capacity(
            self.task.len()
                + self.blocker.as_ref().map_or(0, String::len)
                + self.next_action.as_ref().map_or(0, String::len)
                + 64,
        );
        if !self.task.is_empty() {
            push_compact_summary_part(&mut summary, "", &self.task);
        }
        push_compact_summary_part(&mut summary, "status=", self.status.as_str());
        if let Some(step) = self.active_step_title()
            && !step.is_empty()
        {
            push_compact_summary_part(&mut summary, "active=", step);
        }
        if let Some(blocker) = &self.blocker {
            push_compact_summary_part(&mut summary, "blocker=", blocker);
        }
        if let Some(next_action) = &self.next_action {
            push_compact_summary_part(&mut summary, "next=", next_action);
        }
        push_compact_summary_part(&mut summary, "verification=", self.verification.as_str());
        summary
    }

    pub fn normalized(mut self) -> Self {
        self.task = normalize_task_text(self.task, 500);
        self.summary = normalize_optional_task_text(self.summary, 500);
        self.blocker = normalize_optional_task_text(self.blocker, 500);
        self.next_action = normalize_optional_task_text(self.next_action, 500);
        self.replan_reason = normalize_optional_task_text(self.replan_reason, 500);
        self.steps = self
            .steps
            .into_iter()
            .take(20)
            .map(|mut step| {
                step.title = normalize_task_text(step.title, 200);
                step.detail = normalize_optional_task_text(step.detail, 300);
                step
            })
            .collect();
        self.recent_changes = self
            .recent_changes
            .into_iter()
            .filter_map(|change| normalize_optional_task_text(Some(change), 300))
            .take(20)
            .collect();
        if self.blocker.is_some() && self.status == TaskStateStatus::Running {
            self.status = TaskStateStatus::Blocked;
        }
        self
    }
}

fn push_compact_summary_part(summary: &mut String, prefix: &str, value: &str) {
    if !summary.is_empty() {
        summary.push_str(" | ");
    }
    summary.push_str(prefix);
    summary.push_str(value);
}

fn normalize_optional_task_text(value: Option<String>, limit: usize) -> Option<String> {
    value.and_then(|text| {
        let text = normalize_task_text(text, limit);
        (!text.is_empty()).then_some(text)
    })
}

fn normalize_task_text(text: String, limit: usize) -> String {
    let mut output = text.trim().replace('\n', " ");
    if output.chars().count() > limit {
        output = output.chars().take(limit.saturating_sub(3)).collect();
        output.push_str("...");
    }
    output
}

pub const DEFAULT_INSTRUCTIONS: &str = "You are Squeezy, a cost-aware coding agent. Keep responses concise, explicit, and grounded in workspace evidence. Prefer semantic graph tools such as repo_map, definition_search, symbol_context, reference_search, and read_slice before grep/read_file on supported code. When you would otherwise issue the same grep or read_file repeatedly against one symbol or one type, a single graph call replaces them — reach for it even when grep would also work, because the graph result already follows imports, re-exports, and renamed aliases that regex misses. When a graph result gives you a symbol_id, prefer reading its body with read_slice(symbol_id, span_kind=body) over a whole-file read_file. Use websearch for web discovery and webfetch for retrieving a specific URL when web tools are available. Treat websearch and webfetch results as remote documentation evidence, cite source URLs from their citation metadata when relying on them, and keep remote docs distinct from local code or graph facts. Do not invent URLs. If a tool call is denied, do not retry the same call. Do not issue duplicate tool calls — if you need the same result you already have, refer to the earlier output instead of re-running the call. For simple existence checks (e.g. \"does function X exist?\") or simple definition questions (e.g. \"which file defines X?\"), a single grep or definition_search is usually enough; stop once graph evidence directly answers the user instead of adding repo_map, grep, or relationship tools. Before a batch of two or more related tool calls, emit a brief preamble (1–2 sentences, roughly 8–12 words) saying what you are about to do — for example: \"Looking up Error in src/lib.rs, then tracing its constructors.\" Logically group related tools under one preamble; if a turn covers two unrelated topics, emit one preamble per group. Skip the preamble for a single tool call or a trivial answer.";

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
