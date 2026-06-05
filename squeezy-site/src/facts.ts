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
  eyebrow: "coding agent",
  title: "Your CPU does the deterministic, repetitive work.",
  titleCont: "Your tokens go to the thinking.",
  lead:
    "Squeezy does the deterministic, repetitive work on your machine and saves model tokens for the reasoning that actually needs them. Same coding work, smaller bill.",
  note: "Written in Rust. Bring your own model."
};

export const heroMetrics = [
  { label: "Languages", value: "15", detail: "with local code understanding" },
  { label: "Saving layers", value: "4", detail: "working together on every turn" },
  { label: "Providers", value: "18", detail: "presets, plus any compatible endpoint" },
  { label: "Platforms", value: "3", detail: "macOS, Linux, Windows" }
];

export const genericProof = {
  stat: "Lower cost",
  detail: "across all 15 languages in our same-task benchmark, at about 0.65× the baseline.",
  note: "Measured against other agents on matched tasks. See the benchmark page for the full method and the model-by-model breakdown."
};

export type LeverGroup = {
  id: string;
  label: string;
  title: string;
  why: string;
  levers: FactCard[];
};

// The four saving layers, in narrative order: understand -> reuse -> right-size -> observe.
// Local code understanding is shown first but is co-equal with caching and routing.
export const leverGroups: LeverGroup[] = [
  {
    id: "understand",
    label: "understand first",
    title: "Understand the code first",
    why:
      "Before it asks the model anything, Squeezy reads your repository locally and works out which files and which lines matter.",
    levers: [
      {
        label: "local code understanding",
        title: "Read the relevant code, not the whole file",
        body:
          "Squeezy navigates your code on your machine to find the right declarations, callers, and slices, then sends the model those instead of dumping entire files into the prompt."
      }
    ]
  },
  {
    id: "reuse",
    label: "don't pay twice",
    title: "Don't pay for the same bytes twice",
    why:
      "Most of a coding session repeats: the same instructions, the same files, the same command output. Squeezy keeps that out of the bill.",
    levers: [
      {
        label: "prompt caching",
        title: "Reuse stable context",
        body:
          "Where a provider supports prompt caching, Squeezy keeps stable instructions and tool context cache-friendly so repeated turns are charged less."
      },
      {
        label: "receipts",
        title: "Replace repeated output with a receipt",
        body:
          "When the same file or command result would be sent again, Squeezy sends a small receipt that points back to the earlier result instead of resending the bytes."
      },
      {
        label: "deferred tool schemas",
        title: "Load tool definitions on demand",
        body:
          "The model sees a compact, stable index of tools first and pulls a full tool definition only when it needs one, which also keeps the cached prompt prefix intact."
      }
    ]
  },
  {
    id: "right-size",
    label: "right-size turns",
    title: "Right-size every turn",
    why:
      "Not every turn deserves the biggest model or the longest history. Squeezy matches the effort and the context to the task in front of it.",
    levers: [
      {
        label: "routing",
        title: "Send simple turns to a cheaper model",
        body:
          "Obvious mechanical requests start on the provider's small, fast model and escalate to the main model only when the task turns out to be hard."
      },
      {
        label: "compaction",
        title: "Keep long sessions bounded",
        body:
          "As a conversation grows, older state is folded into a short summary of goal, progress, decisions, and next steps while recent work stays intact, so turn 30 doesn't pay for turns 1 through 29."
      },
      {
        label: "shaped output",
        title: "Send the useful part of command output",
        body:
          "Build, test, search, and diff output is trimmed to the part the model can act on, with the full output kept available if it is needed."
      },
      {
        label: "verbosity",
        title: "Control how much is said",
        body:
          "Response and tool-output verbosity settings keep normal turns concise, with full detail available on request."
      },
      {
        label: "subagents",
        title: "Keep exploration off the main thread",
        body:
          "Short-lived subagents research or review in their own context and hand back a summary, instead of expanding the main conversation."
      }
    ]
  },
  {
    id: "see-the-bill",
    label: "see the bill",
    title: "See the bill",
    why:
      "None of this is a black box. Squeezy shows where the tokens went so you can trust, and tune, the savings.",
    levers: [
      {
        label: "accounting",
        title: "Show where tokens go",
        body:
          "Cost and context views separate input, output, cached input, tool output, and reasoning, with dollar estimates where the provider exposes them."
      }
    ]
  }
];

export const productSubjects: FactCard[] = [
  {
    label: "coding first",
    title: "A coding agent for real code work",
    body:
      "Squeezy can inspect code, edit files, run commands, manage plans, resume sessions, and keep model work tied to local evidence."
  },
  {
    label: "languages",
    title: "Language-aware code understanding",
    body:
      "Fifteen supported languages get local code understanding and navigation before the model reaches for broad file context."
  },
  {
    label: "permissions",
    title: "Reviewable local actions",
    body:
      "File edits, shell commands, web access, MCP calls, destructive actions, and outside-workspace paths stay behind configurable policies."
  },
  {
    label: "sessions",
    title: "Work can be resumed and audited",
    body:
      "Local logs, resume state, reports, labels, forks, and feedback keep long coding sessions inspectable."
  },
  {
    label: "providers",
    title: "Bring your preferred model",
    body:
      "Use native providers, compatible endpoints, OAuth-style routes, or local runtimes while Squeezy keeps the optimization local."
  },
  {
    label: "docs",
    title: "Technical detail stays in docs",
    body:
      "Marketing pages explain outcomes. Documentation covers configuration, permissions, cost receipts, providers, and code navigation internals."
  }
];

export const benchmarkRows: BenchmarkRow[] = [
  { lang: "C", squeezyCost: 0.0454, baselineCost: 0.0504, ratio: 0.9, recall: 100, verdict: "WIN" },
  { lang: "C++", squeezyCost: 0.0557, baselineCost: 0.0689, ratio: 0.81, recall: 100, verdict: "WIN" },
  { lang: "C#", squeezyCost: 0.016, baselineCost: 0.0341, ratio: 0.47, recall: 100, verdict: "WIN" },
  { lang: "Dart", squeezyCost: 0.1049, baselineCost: 0.1802, ratio: 0.58, recall: 100, verdict: "WIN" },
  { lang: "Go", squeezyCost: 0.0222, baselineCost: 0.0477, ratio: 0.47, recall: 100, verdict: "WIN" },
  { lang: "Java", squeezyCost: 0.0488, baselineCost: 0.1094, ratio: 0.45, recall: 100, verdict: "WIN" },
  { lang: "JS", squeezyCost: 0.0122, baselineCost: 0.0182, ratio: 0.67, recall: 100, verdict: "WIN" },
  { lang: "Kotlin", squeezyCost: 0.0271, baselineCost: 0.0416, ratio: 0.65, recall: 100, verdict: "WIN" },
  { lang: "PHP", squeezyCost: 0.0261, baselineCost: 0.0418, ratio: 0.62, recall: 100, verdict: "WIN" },
  { lang: "Python", squeezyCost: 0.0155, baselineCost: 0.0193, ratio: 0.81, recall: 100, verdict: "WIN" },
  { lang: "Ruby", squeezyCost: 0.0134, baselineCost: 0.0496, ratio: 0.27, recall: 100, verdict: "WIN" },
  { lang: "Rust", squeezyCost: 0.0278, baselineCost: 0.0355, ratio: 0.78, recall: 100, verdict: "WIN" },
  { lang: "Scala", squeezyCost: 0.0202, baselineCost: 0.0611, ratio: 0.33, recall: 100, verdict: "WIN" },
  { lang: "Swift", squeezyCost: 0.0134, baselineCost: 0.0181, ratio: 0.74, recall: 100, verdict: "WIN" },
  { lang: "TS", squeezyCost: 0.0378, baselineCost: 0.0424, ratio: 0.89, recall: 100, verdict: "WIN" }
];

export const haikuBenchmarkRows: BenchmarkRow[] = [
  { lang: "C", squeezyCost: 0.0501, baselineCost: 0.0987, ratio: 0.51, recall: 100, verdict: "WIN" },
  { lang: "C++", squeezyCost: 0.1707, baselineCost: 0.2074, ratio: 0.82, recall: 100, verdict: "WIN" },
  { lang: "C#", squeezyCost: 0.2242, baselineCost: 0.2364, ratio: 0.95, recall: 100, verdict: "WIN" },
  { lang: "Dart", squeezyCost: 0.1326, baselineCost: 0.2275, ratio: 0.58, recall: 100, verdict: "WIN" },
  { lang: "Go", squeezyCost: 0.0141, baselineCost: 0.1487, ratio: 0.09, recall: 100, verdict: "WIN" },
  { lang: "Java", squeezyCost: 0.267, baselineCost: 0.3696, ratio: 0.72, recall: 100, verdict: "WIN" },
  { lang: "JS", squeezyCost: 0.0404, baselineCost: 0.0549, ratio: 0.74, recall: 100, verdict: "WIN" },
  { lang: "Kotlin", squeezyCost: 0.1159, baselineCost: 0.2038, ratio: 0.57, recall: 100, verdict: "WIN" },
  { lang: "PHP", squeezyCost: 0.0499, baselineCost: 0.1083, ratio: 0.46, recall: 100, verdict: "WIN" },
  { lang: "Python", squeezyCost: 0.058, baselineCost: 0.1074, ratio: 0.54, recall: 100, verdict: "WIN" },
  { lang: "Ruby", squeezyCost: 0.2178, baselineCost: 0.2963, ratio: 0.73, recall: 100, verdict: "WIN" },
  { lang: "Rust", squeezyCost: 0.0858, baselineCost: 0.1509, ratio: 0.57, recall: 80, verdict: "WIN" },
  { lang: "Scala", squeezyCost: 0.1959, baselineCost: 0.2884, ratio: 0.68, recall: 100, verdict: "WIN" },
  { lang: "Swift", squeezyCost: 0.0215, baselineCost: 0.0342, ratio: 0.63, recall: 100, verdict: "WIN" },
  { lang: "TS", squeezyCost: 0.0791, baselineCost: 0.0996, ratio: 0.79, recall: 100, verdict: "WIN" }
];

export const benchmarkSummary = {
  codexWins: "15 / 15",
  claudeWins: "15 / 15",
  codexModel: "Squeezy gpt-5.4-mini vs Codex gpt-5.4-mini",
  claudeModel: "Squeezy claude-haiku-4-5 vs Claude Code haiku",
  runs: "n=10 medians",
  totalDelta: "lower model spend",
  medianRatio: "0.65",
  suite:
    "same-task real-world code-navigation benchmark, equal pricing and grader, Squeezy versus Codex on the Mini tier and Claude Code on the Haiku tier.",
  source:
    "docs/internal/eval-findings/board-and-graph-fixes-summary.md"
};

export const operatingLoop: FactCard[] = [
  {
    label: "1",
    title: "Understand the repo locally",
    body:
      "Squeezy builds a local understanding of your code and workspace so the first model call does not start from a blank repository."
  },
  {
    label: "2",
    title: "Read only the relevant code",
    body:
      "The agent narrows broad questions into specific files, symbols, diffs, command outputs, or verifier steps."
  },
  {
    label: "3",
    title: "Keep context tight",
    body:
      "Repeated output is replaced with receipts, noisy output is shaped, and long conversations are compacted before they become expensive."
  },
  {
    label: "4",
    title: "Send focused work to the model",
    body:
      "The selected provider gets the useful context, and Squeezy tracks tokens, cache usage, tool output, and estimated spend."
  }
];

export const languageRows: MatrixRow[] = [
  { name: "Rust", detail: "Cargo workspaces, crates, traits, impls, modules, and tests." },
  { name: "Python", detail: "Packages, imports, classes, functions, decorators, and inheritance." },
  { name: "Java", detail: "Packages, Maven/Gradle projects, classes, members, and inheritance." },
  { name: "Kotlin", detail: "Packages, Gradle projects, classes, objects, companions, and extensions." },
  { name: "Scala", detail: "Packages, traits, objects, case classes, enums, and extension methods." },
  { name: "C#/.NET", detail: "Solutions, namespaces, usings, partial types, attributes, and members." },
  { name: "Go", detail: "Modules, packages, structs, interfaces, receivers, imports, and tests." },
  { name: "C", detail: "Headers, includes, structs, functions, typedefs, macros, and references." },
  { name: "C++", detail: "Headers, namespaces, classes, templates, methods, and overload-heavy code." },
  { name: "JavaScript", detail: "ES modules, CommonJS, functions, classes, exports, and JSX." },
  { name: "TypeScript", detail: "Types, interfaces, imports, generics, classes, and TSX." },
  { name: "PHP", detail: "Namespaces, Composer-style code, traits, enums, attributes, and methods." },
  { name: "Ruby", detail: "Classes, modules, mixins, singleton methods, accessors, and require paths." },
  { name: "Swift", detail: "Modules, protocols, actors, structs, extensions, and property wrappers." },
  { name: "Dart", detail: "Libraries, parts, classes, mixins, extensions, and Flutter-style projects." }
];

export const providerGroups: MatrixRow[] = [
  {
    name: "Native providers",
    detail:
      "OpenAI, Anthropic, Google Gemini, Azure OpenAI, AWS Bedrock, and Ollama have dedicated or local runtime paths.",
    status: "API keys or local config"
  },
  {
    name: "Compatible APIs",
    detail:
      "OpenRouter, Vercel AI Gateway, PortKey, Groq, xAI, DeepSeek, Mistral, Together, Fireworks, Cerebras, and any other OpenAI-compatible endpoint.",
    status: "bring an endpoint"
  },
  {
    name: "Local runtimes",
    detail:
      "Use local or self-hosted routes such as Ollama, LM Studio, vLLM, llama.cpp-style servers, or custom compatible base URLs.",
    status: "local when configured"
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
    status: "GEMINI_API_KEY"
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
    detail: "x86_64 MSVC archive and Winget manifest update path. Some sandbox behavior is platform-limited on Windows.",
    status: "x86_64"
  }
];

