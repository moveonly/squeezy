export type FactCard = {
  label: string;
  title: string;
  body: string;
};

export type MatrixRow = {
  name: string;
  detail: string;
  status?: string;
};

export const productPosition = {
  eyebrow: "Local-first coding agent",
  title: "Understand the repo before you ask the model.",
  lead:
    "Squeezy is a terminal coding agent that builds a local semantic graph of your codebase, then answers navigation, reference, and impact questions from that graph instead of from paid model context."
};

export const homepageCards: FactCard[] = [
  {
    label: "Local first",
    title: "Static analysis does the repetitive work",
    body:
      "Squeezy maps the repository locally, then answers common navigation questions from the graph before the model touches source text."
  },
  {
    label: "Token budget",
    title: "Less context sent without losing the trail",
    body:
      "Graph tools return paths, spans, hashes, confidence labels, provenance, and next actions. Raw reads stay available, narrowed to the exact slice when structure is enough."
  },
  {
    label: "Terminal app",
    title: "Fast local agent loop",
    body:
      "Squeezy runs as a single Rust binary with deterministic local work, explicit verification, and bounded tool output."
  }
];

export const optimizationCards: FactCard[] = [
  {
    label: "static graph",
    title: "Semantic navigation before file reads",
    body:
      "repo_map, declaration search, references, hierarchy, symbol context, upstream/downstream flow, and read_slice work from local graph state and return narrowed results with paths, spans, and confidence."
  },
  {
    label: "read shaping",
    title: "Exact slices, diff reads, and receipts",
    body:
      "Read tools return bounded slices, changed ranges, receipt stubs for unchanged content, and spill handles for large output. The model gets enough to act without paying for repeated bytes."
  },
  {
    label: "tool budget",
    title: "Budget counters visible in the session",
    body:
      "Per-turn counters track tool calls, read bytes, search hits, receipt hits, spills, denials, provider tokens, cache usage, and estimated cost when a provider exposes enough data."
  }
];

export const operatingLoop: FactCard[] = [
  {
    label: "1",
    title: "Index local code",
    body:
      "Squeezy parses supported files, discovers workspace facts, stores graph/cache partitions, and refreshes graph state as the workspace changes."
  },
  {
    label: "2",
    title: "Compile a focused evidence plan",
    body:
      "Common navigation prompts are routed through graph-first plans so the model starts with declarations, references, callers, hierarchy, and exact next actions."
  },
  {
    label: "3",
    title: "Escalate only when needed",
    body:
      "If graph evidence is incomplete, Squeezy falls back to bounded grep, glob, read_file, web, shell, or compiler tools behind the configured permission policy."
  },
  {
    label: "4",
    title: "Verify with local tools",
    body:
      "Builds, tests, formatters, linters, and benchmark commands provide compiler-backed evidence when the task needs it."
  }
];

export const toolSurface: FactCard[] = [
  {
    label: "navigation",
    title: "Graph-backed code tools",
    body:
      "Architecture maps, declarations, definitions, references, call candidates, hierarchy, symbol context, dependency flow, diff context, and exact read slices."
  },
  {
    label: "mutation",
    title: "Plan, patch, verify",
    body:
      "Plan mode hides mutation. Build mode exposes edit, shell, compiler, and git-style actions through capability checks, output shaping, and optional checkpoints."
  },
  {
    label: "support",
    title: "Local help, sessions, reports",
    body:
      "Squeezy can answer questions about itself from bundled docs before provider work, resume sessions, export/replay session logs, and prepare redacted feedback or report bundles."
  }
];

export const languageRows: MatrixRow[] = [
  {
    name: "Rust",
    detail: "Modules, traits, impls, references, calls, tests, and crate facts from cargo metadata.",
    status: "first-class graph"
  },
  {
    name: "Python",
    detail: "Classes, functions, imports, decorators, bases, annotations, exports, and references.",
    status: "first-class graph"
  },
  {
    name: "Java",
    detail: "Packages, types, members, inheritance, implements edges, calls, references, and Maven/Gradle facts.",
    status: "first-class graph"
  },
  {
    name: "Kotlin",
    detail: "Packages, imports (aliased/wildcard), classes, objects, companion objects, data/sealed types, extension functions, suspend, typealiases, and Gradle facts.",
    status: "first-class graph"
  },
  {
    name: "C#",
    detail: "Namespaces, types, members, partial links, inheritance, references, and .csproj/.sln project facts.",
    status: "first-class graph"
  },
  {
    name: "Go",
    detail: "Packages, structs, interfaces, methods, receivers, tests, calls, and references.",
    status: "first-class graph"
  },
  {
    name: "C",
    detail: "Includes, structs, unions, enums, typedefs, functions, macros, and references.",
    status: "first-class graph"
  },
  {
    name: "C++",
    detail: "Includes, namespaces, classes, methods, constructors, destructors, templates, operators, and references.",
    status: "first-class graph"
  },
  {
    name: "JavaScript",
    detail: "Imports, exports, CommonJS aliases, functions, classes, member references, calls, and JSX declarations.",
    status: "first-class graph"
  },
  {
    name: "TypeScript",
    detail: "Imports, exports, classes, interfaces, type aliases, enums, decorators, type references, calls, and TSX declarations.",
    status: "first-class graph"
  },
  {
    name: "PHP",
    detail: "Namespaces, use imports (named/aliased/group/function/const), classes, interfaces, traits, enums, backed enums, methods, properties, attributes, calls, and references.",
    status: "first-class graph"
  },
  {
    name: "Ruby",
    detail: "Classes, modules, methods, singleton methods, attr_* synthesis, require_relative imports, include/extend/prepend mixins, calls, and references.",
    status: "first-class graph"
  },
  {
    name: "Swift",
    detail: "Imports, classes, structs, actors, protocols, enums with associated cases, extensions, generics with constraints, property wrappers, and module hints.",
    status: "first-class graph"
  }
];

export const aggregatorRows: MatrixRow[] = [
  {
    name: "OpenRouter",
    detail: "One credit, every frontier model. OpenAI-compatible Chat Completions streaming, function tools, usage metadata, reasoning passthrough, Anthropic cache_control forwarding. Default model: anthropic/claude-opus-4-7.",
    status: "OPENROUTER_API_KEY"
  },
  {
    name: "Vercel AI Gateway",
    detail: "Vercel's routing proxy for OpenAI, Anthropic, Google, xAI, and more. Default model: anthropic/claude-opus-4-7.",
    status: "AI_GATEWAY_API_KEY"
  },
  {
    name: "PortKey",
    detail: "Gateway with virtual keys, caching, and observability. Configure a virtual key via x-portkey-virtual-key header.",
    status: "PORTKEY_API_KEY"
  }
];

export const providerRows: MatrixRow[] = [
  {
    name: "OpenAI",
    detail: "Responses streaming, function tools, cached-token usage. Default model: gpt-5.5, 400K context.",
    status: "OPENAI_API_KEY"
  },
  {
    name: "Anthropic",
    detail: "Messages streaming, function tools, cache read/write usage. Default model: claude-opus-4-7, 200K context.",
    status: "ANTHROPIC_API_KEY"
  },
  {
    name: "Google Gemini",
    detail: "streamGenerateContent SSE, function declarations, usage metadata. Default model: gemini-2.5-pro, 1M context.",
    status: "GEMINI_API_KEY"
  }
];

export const cloudPlatformRows: MatrixRow[] = [
  {
    name: "Amazon Bedrock",
    detail: "AWS multi-vendor catalog (Anthropic, Meta, Mistral, Amazon, Cohere, AI21, Stability). Bedrock Runtime ConverseStream over the AWS default credential chain. Default model: Claude Haiku 4.5, 200K context.",
    status: "AWS credentials"
  },
  {
    name: "Azure OpenAI",
    detail: "OpenAI models hosted on Microsoft Azure (single-vendor slice of the broader Azure model catalog). Responses-compatible streaming with api-key auth and api-version. Default model: gpt-5.5, 400K context. For Azure AI Foundry's multi-vendor catalog use the openai_compatible preset with the Foundry serverless endpoint.",
    status: "AZURE_OPENAI_API_KEY"
  },
  {
    name: "Google Vertex AI",
    detail: "Gemini and other models on Google Cloud Vertex AI's OpenAI-compatible endpoint. OAuth2 access token (refresh every ~50 min) or service-account JSON, project + location templated into base_url. Default model: gemini-2.5-pro.",
    status: "VERTEX_ACCESS_TOKEN"
  }
];

export const localRuntimeRows: MatrixRow[] = [
  {
    name: "Ollama",
    detail: "Local /api/chat NDJSON streaming with function tool schemas. Default model: qwen3-coder, runtime-defined context.",
    status: "local runtime"
  }
];

export const openAiCompatibleRows: MatrixRow[] = [
  {
    name: "Groq",
    detail: "Fast OpenAI-compatible inference of open models. Curated registry: llama-3.3-70b-versatile, llama-3.1-8b-instant, moonshotai/kimi-k2-instruct.",
    status: "GROQ_API_KEY"
  },
  {
    name: "xAI",
    detail: "Grok family via OpenAI-compatible API. Curated registry: grok-4, grok-4-fast-reasoning, grok-code-fast-1.",
    status: "XAI_API_KEY"
  },
  {
    name: "DeepSeek",
    detail: "DeepSeek Chat and Reasoner via OpenAI-compatible API. Curated registry: deepseek-chat, deepseek-reasoner.",
    status: "DEEPSEEK_API_KEY"
  },
  {
    name: "Mistral La Plateforme",
    detail: "Mistral's OpenAI-compatible Chat Completions endpoint. Default model: mistral-large-latest (light preset — no curated registry entries).",
    status: "MISTRAL_API_KEY"
  },
  {
    name: "Together AI",
    detail: "Hosted open models via OpenAI-compatible API. Default model: meta-llama/Llama-3.3-70B-Instruct-Turbo (light preset).",
    status: "TOGETHER_API_KEY"
  },
  {
    name: "Fireworks AI",
    detail: "Production OpenAI-compatible inference. Default model: accounts/fireworks/models/llama-v3p3-70b-instruct (light preset).",
    status: "FIREWORKS_API_KEY"
  },
  {
    name: "Cerebras",
    detail: "Cerebras Cloud OpenAI-compatible endpoint. Default model: llama-3.3-70b (light preset).",
    status: "CEREBRAS_API_KEY"
  },
  {
    name: "Custom OpenAI-compatible",
    detail: "Any endpoint that speaks POST /chat/completions with a Bearer token. Microsoft Foundry (Azure AI Studio) serverless deployments, Cloudflare Workers AI, self-hosted LiteLLM, and similar services use this preset.",
    status: "openai_compatible preset"
  }
];

export const installRows: MatrixRow[] = [
  {
    name: "macOS",
    detail: "Release targets: aarch64-apple-darwin for Apple Silicon and x86_64-apple-darwin for Intel. Primary install path: one-line curl installer.",
    status: "curl installer"
  },
  {
    name: "Linux",
    detail: "x86_64-unknown-linux-musl static binary. Primary install path: one-line curl installer.",
    status: "curl installer"
  },
  {
    name: "Windows",
    detail: "x86_64-pc-windows-msvc archive. Primary install path: Winget.",
    status: "winget"
  },
  {
    name: "Source build",
    detail: "Cargo install is available when you already have the required Rust toolchain.",
    status: "cargo"
  }
];

export const benchmarkFacts: FactCard[] = [
  {
    label: "scope",
    title: "Cost-saving benchmark page is under construction",
    body:
      "The public page will report how much context Squeezy avoids by using graph navigation, exact reads, receipts, and output shaping before model turns."
  },
  {
    label: "method",
    title: "Benchmarks need quality and cost together",
    body:
      "The benchmark method will pair navigation quality with measured read bytes, tool calls, receipt hits, spills, provider tokens, and baseline discovery effort."
  },
  {
    label: "status",
    title: "No public savings number yet",
    body:
      "Until the report is ready, the page should explain the measurement plan without publishing unsupported percentages or cost claims."
  }
];
