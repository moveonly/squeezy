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

export type BenchmarkRow = {
  lang: string;
  squeezyCost: number;
  baselineCost: number;
  ratio: number;
  recall: number;
  verdict: "WIN" | "LOSS";
};

export const productPosition = {
  eyebrow: "Rust coding agent",
  title: "Spend local code understanding before model context.",
  lead:
    "Squeezy is a terminal coding agent that builds a local semantic graph, uses focused tools, and keeps long sessions bounded so paid model tokens are spent on judgment instead of rediscovering the repo."
};

export const heroMetrics = [
  { label: "Measured suite", value: "20% lower total spend", detail: "15-language Mini benchmark" },
  { label: "Languages", value: "13 families", detail: "17 source variants with graph navigation" },
  { label: "Runtime", value: "Rust TUI", detail: "local graph, store, tools, and sessions" },
  { label: "Providers", value: "BYO model", detail: "native, OAuth, local, and compatible routes" }
];

export const costPillars: FactCard[] = [
  {
    label: "graph first",
    title: "Navigate structure before reading files",
    body:
      "Declarations, references, callers, callees, hierarchy, body hits, and exact slices come from a local tree-sitter graph for supported languages."
  },
  {
    label: "bounded evidence",
    title: "Small packets with recovery paths",
    body:
      "Tool outputs are capped, shaped, deduped, or spilled behind handles so the model sees useful evidence without carrying bulk forever."
  },
  {
    label: "cache-aware",
    title: "Stable prompt bytes when providers support it",
    body:
      "Provider adapters wire cache hints and parse cache usage where available, while lazy schemas and skills keep rarely used instructions out of normal turns."
  },
  {
    label: "dynamic context",
    title: "Compact the transcript before it becomes baggage",
    body:
      "Long sessions use compaction and micro-compaction to summarize stale context while preserving receipts and ways to fetch exact detail again."
  },
  {
    label: "routing",
    title: "Use cheaper models for narrow mechanical turns",
    body:
      "The router can choose a provider-local small-fast model for obvious tasks, then escalate back to the parent model when the cheap path shows stress."
  },
  {
    label: "subagents",
    title: "Isolate exploration from the parent context",
    body:
      "Short-lived read-oriented subagents can research, plan, review, or answer docs questions and return compact evidence instead of a full child transcript."
  }
];

export const productSubjects: FactCard[] = [
  {
    label: "coding first",
    title: "Graph-backed tools for code work",
    body:
      "The core tool surface is declarations, references, flow, hierarchy, diff context, read slices, patch planning, shell verification, and session accounting."
  },
  {
    label: "tui workflows",
    title: "Built for repeated terminal sessions",
    body:
      "First-run setup, config screens, prompt queues, resume picker, plan/build modes, cost/context commands, and keymap controls live in the TUI."
  },
  {
    label: "permissions",
    title: "Reviewable actions around local work",
    body:
      "File edits, shell commands, web fetches, MCP calls, destructive actions, and outside-workspace access flow through configurable permissions."
  },
  {
    label: "sessions",
    title: "Local work can be resumed and audited",
    body:
      "Session logs, resume state, labels, forks, exports, feedback, reports, and optional checkpoints make longer work inspectable after restarts."
  },
  {
    label: "mcp and web",
    title: "External tools stay explicit",
    body:
      "Web and MCP tools are configured, permissioned, bounded, and separated from local code evidence instead of being implicit background access."
  },
  {
    label: "validation",
    title: "Benchmarks check the graph against oracles",
    body:
      "Release benchmarks compare local graph output with language-specific compiler or language-service oracles while production navigation stays local."
  }
];

export const benchmarkRows: BenchmarkRow[] = [
  { lang: "C", squeezyCost: 0.0454, baselineCost: 0.0504, ratio: 0.9, recall: 100, verdict: "WIN" },
  { lang: "C++", squeezyCost: 0.0557, baselineCost: 0.0689, ratio: 0.81, recall: 100, verdict: "WIN" },
  { lang: "C#", squeezyCost: 0.0636, baselineCost: 0.0525, ratio: 1.21, recall: 100, verdict: "LOSS" },
  { lang: "Dart", squeezyCost: 0.1049, baselineCost: 0.1802, ratio: 0.58, recall: 100, verdict: "WIN" },
  { lang: "Go", squeezyCost: 0.0479, baselineCost: 0.0486, ratio: 0.99, recall: 100, verdict: "LOSS" },
  { lang: "Java", squeezyCost: 0.1441, baselineCost: 0.1499, ratio: 0.96, recall: 100, verdict: "LOSS" },
  { lang: "JS", squeezyCost: 0.0552, baselineCost: 0.065, ratio: 0.85, recall: 100, verdict: "WIN" },
  { lang: "Kotlin", squeezyCost: 0.0271, baselineCost: 0.0416, ratio: 0.65, recall: 100, verdict: "WIN" },
  { lang: "PHP", squeezyCost: 0.0261, baselineCost: 0.0418, ratio: 0.62, recall: 100, verdict: "WIN" },
  { lang: "Python", squeezyCost: 0.0155, baselineCost: 0.0193, ratio: 0.81, recall: 100, verdict: "WIN" },
  { lang: "Ruby", squeezyCost: 0.0617, baselineCost: 0.0607, ratio: 1.02, recall: 100, verdict: "LOSS" },
  { lang: "Rust", squeezyCost: 0.0278, baselineCost: 0.0355, ratio: 0.78, recall: 100, verdict: "WIN" },
  { lang: "Scala", squeezyCost: 0.0202, baselineCost: 0.0611, ratio: 0.33, recall: 100, verdict: "WIN" },
  { lang: "Swift", squeezyCost: 0.0134, baselineCost: 0.0181, ratio: 0.74, recall: 100, verdict: "WIN" },
  { lang: "TS", squeezyCost: 0.0378, baselineCost: 0.0424, ratio: 0.89, recall: 100, verdict: "WIN" }
];

export const benchmarkSummary = {
  totalDelta: "20%",
  wins: 11,
  losses: 4,
  medianRatio: "0.81",
  suite:
    "15 real-world code-navigation tasks using gpt-5.4-mini, n=3 medians, identical pricing and grader, Squeezy graph-enabled agent versus Codex baseline."
};

export const accuracyRows: MatrixRow[] = [
  {
    name: "Rust",
    detail:
      "Five pinned repos and 25,000 mixed-workload scenarios. Comparable declaration symbols matched rust-analyzer with 20,320 TP / 0 FP / 0 FN in the recorded run.",
    status: "oracle checked"
  },
  {
    name: "Java",
    detail:
      "Five external Java repos checked against JDK compiler-tree declaration oracles: 107,063 TP / 4 FP / 8 FN aggregated.",
    status: "declaration oracle"
  },
  {
    name: "Go",
    detail:
      "Five external Go repos checked against Go parser/types oracle: 29,038 TP / 0 FP / 0 FN, with refresh probes reparsing only edited files.",
    status: "declaration oracle"
  }
];

export const operatingLoop: FactCard[] = [
  {
    label: "1",
    title: "Index local code",
    body:
      "Parse supported files, classify workspace facts, build graph/cache partitions, and refresh changed files as the repo moves."
  },
  {
    label: "2",
    title: "Plan with graph evidence",
    body:
      "Route navigation prompts through declarations, references, callers, hierarchy, body hits, diff context, and exact next reads."
  },
  {
    label: "3",
    title: "Spend model context selectively",
    body:
      "Send focused evidence and escalate to raw reads, shell, web, MCP, or compiler tools only when local structure is not enough."
  },
  {
    label: "4",
    title: "Account and compact",
    body:
      "Track provider tokens, cache counters, tool bytes, spills, subagent cost, and compact stale transcript bulk during longer work."
  }
];

export const languageRows: MatrixRow[] = [
  { name: "Rust", detail: "Modules, traits, impls, macros, tests, calls, references, and Cargo facts.", status: "strong" },
  { name: "Python", detail: "Classes, functions, imports, decorators, bases, annotations, calls, exports, and references.", status: "solid" },
  { name: "Java", detail: "Packages, types, members, constructors, inheritance, implements edges, calls, references, and Maven/Gradle facts.", status: "solid" },
  { name: "Kotlin", detail: "Packages, imports, classes, objects, companion objects, sealed/data types, extension receivers, and Gradle facts.", status: "solid" },
  { name: "Scala", detail: "Packages, classes, traits, objects, case classes, enums, extension methods, givens, and references.", status: "emerging" },
  { name: "C#/.NET", detail: "Namespaces, usings, types, members, attributes, calls, references, partial links, inheritance, and project facts.", status: "strong" },
  { name: "Go", detail: "Packages, imports, structs, interfaces, aliases, functions, methods, receivers, calls, tests, and references.", status: "strong" },
  { name: "C/C++", detail: "Includes, namespaces, classes, structs, unions, enums, typedefs, functions, methods, templates, macros, and references.", status: "strong" },
  { name: "JavaScript/TypeScript", detail: "Imports, exports, CommonJS aliases, functions, classes, interfaces, types, JSX/TSX declarations, member calls, and references.", status: "strong" },
  { name: "PHP", detail: "Namespaces, use imports, classes, interfaces, traits, enums, methods, properties, attributes, calls, references, and trait edges.", status: "strong" },
  { name: "Ruby", detail: "Classes, modules, methods, singleton methods, attr accessors, require/import hints, mixins, calls, and references.", status: "solid" },
  { name: "Swift", detail: "Classes, structs, actors, protocols, enums, extensions, generics, property wrappers, imports, and module hints.", status: "emerging" },
  { name: "Dart", detail: "Libraries, parts, classes, mixins, extensions, extension types, sealed types, enums, constructors, imports, calls, and type refs.", status: "emerging" }
];

export const providerGroups: MatrixRow[] = [
  {
    name: "Native APIs",
    detail:
      "OpenAI, Anthropic, Google Gemini, Azure OpenAI, AWS Bedrock, and Ollama have dedicated runtime paths or local runtime handling.",
    status: "first-party routes"
  },
  {
    name: "OAuth subscriptions",
    detail:
      "Anthropic, OpenAI Codex/ChatGPT subscription, and GitHub Copilot auth flows are represented in the CLI and provider registry.",
    status: "login flows"
  },
  {
    name: "OpenAI-compatible services",
    detail:
      "OpenRouter, Vercel, PortKey, Groq, xAI, DeepSeek, Vertex, Mistral, Together, Fireworks, Cerebras, DeepInfra, Baseten, Cloudflare Workers AI, local LM Studio/vLLM/llama.cpp, and custom compatible endpoints.",
    status: "presets"
  }
];

export const aggregatorRows: MatrixRow[] = [
  {
    name: "OpenRouter",
    detail: "OpenAI-compatible aggregator route with many hosted models. Pricing and cache support depend on the selected model and registry metadata.",
    status: "OPENROUTER_API_KEY"
  },
  {
    name: "Vercel AI Gateway",
    detail: "OpenAI-compatible gateway route for hosted model access through Vercel.",
    status: "AI_GATEWAY_API_KEY"
  },
  {
    name: "PortKey",
    detail: "OpenAI-compatible gateway route for virtual keys, routing, and observability.",
    status: "PORTKEY_API_KEY"
  }
];

export const providerRows: MatrixRow[] = [
  {
    name: "OpenAI",
    detail: "Native OpenAI route with usage parsing and cache-related request metadata where supported.",
    status: "OPENAI_API_KEY"
  },
  {
    name: "Anthropic",
    detail: "Native Anthropic route with API-key and OAuth credential paths plus cache read/write accounting where exposed.",
    status: "ANTHROPIC_API_KEY"
  },
  {
    name: "Google Gemini",
    detail: "Native Gemini route with API-key configuration and streaming usage metadata where available.",
    status: "GOOGLE_API_KEY"
  }
];

export const cloudPlatformRows: MatrixRow[] = [
  {
    name: "Amazon Bedrock",
    detail: "AWS-hosted provider route using the AWS credential chain and Bedrock runtime APIs.",
    status: "AWS credentials"
  },
  {
    name: "Azure OpenAI",
    detail: "Azure-hosted OpenAI route with deployment-specific endpoint and API-key or bearer-token configuration.",
    status: "AZURE_OPENAI_API_KEY"
  },
  {
    name: "Google Vertex AI",
    detail: "Google Cloud route through an OpenAI-compatible endpoint with access-token or service-account OAuth support.",
    status: "Google Cloud auth"
  }
];

export const localRuntimeRows: MatrixRow[] = [
  {
    name: "Ollama",
    detail: "Local runtime route for models served by Ollama. Context and model availability are runtime-defined.",
    status: "local runtime"
  }
];

export const openAiCompatibleRows: MatrixRow[] = [
  {
    name: "Groq, xAI, DeepSeek",
    detail: "Hosted OpenAI-compatible presets with API-key configuration and curated registry entries where available.",
    status: "API key"
  },
  {
    name: "Mistral, Together, Fireworks, Cerebras",
    detail: "OpenAI-compatible hosted inference presets. Dollar estimates require matching pricing metadata.",
    status: "API key"
  },
  {
    name: "Custom and local compatible endpoints",
    detail: "Custom OpenAI-compatible base URLs, plus local LM Studio, vLLM, and llama.cpp style routes.",
    status: "preset"
  }
];

export const installRows: MatrixRow[] = [
  {
    name: "macOS",
    detail: "Release targets for Apple Silicon and Intel. Primary path is the one-line installer; Homebrew tap support is scripted.",
    status: "aarch64 + x86_64"
  },
  {
    name: "Linux",
    detail: "x86_64 musl static binary target plus source install when the Rust toolchain is already present.",
    status: "x86_64"
  },
  {
    name: "Windows",
    detail: "x86_64 MSVC archive and Winget manifest update path. Shell sandboxing is Job-Object-only on Windows.",
    status: "x86_64"
  }
];

export const trustRows: MatrixRow[] = [
  {
    name: "Permissions",
    detail:
      "Configurable policies cover edits, shell, web, MCP, destructive actions, and outside-workspace paths. Defaults keep external access reviewable.",
    status: "reviewable"
  },
  {
    name: "Telemetry",
    detail:
      "Website events go through a Cloudflare Worker to PostHog. Product telemetry is bounded and opt-controlled; reports and feedback are explicit.",
    status: "worker proxy"
  },
  {
    name: "Rollback",
    detail:
      "Optional checkpoints can record mutating tool calls and support call-level or turn-level rollback when a provider is configured.",
    status: "optional"
  }
];
