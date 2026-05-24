use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Public documentation site, surfaced in help bodies and refusal text. Keep stable;
/// renames change user-visible output.
pub const SQUEEZY_WEBSITE_URL: &str = "https://squeezyagent.com/docs/";
/// Public repository URL, surfaced in help bodies and refusal text. Keep stable;
/// renames change user-visible output.
pub const SQUEEZY_REPO_URL: &str = "https://github.com/esqueezy/squeezy";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelpAnswer {
    pub topic: String,
    pub status: HelpStatus,
    pub body: String,
    pub citations: Vec<HelpCitation>,
    pub config_sections: Vec<String>,
}

impl HelpAnswer {
    pub fn render_markdown(&self) -> String {
        let mut output = self.body.clone();
        if !self.citations.is_empty() {
            output.push_str("\n\nCitations:\n");
            for citation in &self.citations {
                output.push_str("- ");
                output.push_str(&citation.render());
                output.push('\n');
            }
        }
        output.trim_end().to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelpStatus {
    Answered,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum HelpCitation {
    DocsPath(String),
    ConfigInspectSection(String),
}

impl HelpCitation {
    fn render(&self) -> String {
        match self {
            Self::DocsPath(path) => format!("docs path: {path}"),
            Self::ConfigInspectSection(section) => {
                format!("config inspect section: [{section}]")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SqueezyHelp {
    config_inspect: String,
}

impl SqueezyHelp {
    pub fn new(config_inspect: impl Into<String>) -> Self {
        Self {
            config_inspect: config_inspect.into(),
        }
    }

    pub fn topic_index(&self) -> HelpAnswer {
        let topics = TOPICS
            .iter()
            .map(|topic| format!("- `{}`: {}", topic.id, topic.title))
            .collect::<Vec<_>>()
            .join("\n");
        HelpAnswer {
            topic: "index".to_string(),
            status: HelpStatus::Answered,
            body: format!(
                "Squeezy help is the first-line support path for questions about Squeezy itself. It answers from bundled docs and this run's redacted `config inspect` output before any model or network lookup.\n\nSupported topics:\n{topics}\n\nUse `/help <topic>` for a local answer. For broader or current public information, use {SQUEEZY_WEBSITE_URL} or {SQUEEZY_REPO_URL}; if external lookup tools are enabled, ask Squeezy to search public docs or the repo."
            ),
            citations: vec![
                HelpCitation::DocsPath("docs/README.md".to_string()),
                HelpCitation::DocsPath("docs/SKILLS.md".to_string()),
            ],
            config_sections: Vec::new(),
        }
    }

    pub fn answer_topic(&self, topic: &str) -> HelpAnswer {
        let Some(definition) = find_topic(topic) else {
            return self.unsupported(topic);
        };
        self.answer_definition(definition)
    }

    pub fn answer_for_input(&self, input: &str) -> Option<HelpAnswer> {
        let trimmed = input.trim();
        if let Some(topic) = parse_help_command(trimmed) {
            return Some(if topic.is_empty() {
                self.topic_index()
            } else {
                self.answer_topic(topic)
            });
        }
        if !looks_like_squeezy_help_question(trimmed) {
            return None;
        }
        let Some(topic) = best_topic_for_text(trimmed) else {
            return Some(self.unsupported(trimmed));
        };
        Some(self.answer_definition(topic))
    }

    fn answer_definition(&self, definition: &TopicDefinition) -> HelpAnswer {
        let extracted_sections = extract_config_sections(&self.config_inspect, definition.config);
        let mut body = format!(
            "Squeezy help: {}\n\n{}",
            definition.title, definition.summary
        );
        if !extracted_sections.is_empty() {
            body.push_str("\n\nRelevant redacted `config inspect` output:\n```toml\n");
            for section in &extracted_sections {
                body.push_str(section.content.trim_end());
                body.push_str("\n\n");
            }
            body.push_str("```");
        }
        body.push_str(
            "\n\nThis answer is limited to local Squeezy docs and config inspect output.",
        );

        let mut citations = definition
            .docs
            .iter()
            .map(|path| HelpCitation::DocsPath((*path).to_string()))
            .collect::<Vec<_>>();
        citations.extend(
            extracted_sections
                .iter()
                .map(|section| HelpCitation::ConfigInspectSection(section.name.clone())),
        );

        HelpAnswer {
            topic: definition.id.to_string(),
            status: HelpStatus::Answered,
            body,
            citations,
            config_sections: extracted_sections
                .into_iter()
                .map(|section| section.name)
                .collect(),
        }
    }

    fn unsupported(&self, topic: &str) -> HelpAnswer {
        let suggestions = TOPICS
            .iter()
            .map(|topic| topic.id)
            .collect::<Vec<_>>()
            .join(", ");
        HelpAnswer {
            topic: topic.trim().to_string(),
            status: HelpStatus::Unsupported,
            body: format!(
                "I don't have local Squeezy help coverage for `{}`.\n\nBuilt-in Squeezy help only answers from bundled docs and redacted `config inspect` output, so I won't guess. Try one of these local topics: {suggestions}.\n\nFor broader or current public information, use {SQUEEZY_WEBSITE_URL} or {SQUEEZY_REPO_URL}. If external lookup tools are enabled, ask Squeezy to search public docs or the repo.",
                topic.trim()
            ),
            citations: Vec::new(),
            config_sections: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TopicDefinition {
    id: &'static str,
    title: &'static str,
    aliases: &'static [&'static str],
    summary: &'static str,
    docs: &'static [&'static str],
    config: &'static [&'static str],
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigSection {
    name: String,
    content: String,
}

const TOPICS: &[TopicDefinition] = &[
    TopicDefinition {
        id: "config",
        title: "configuration and source precedence",
        aliases: &[
            "config",
            "configuration",
            "settings",
            "config inspect",
            "squeezy.toml",
            "settings.toml",
        ],
        summary: "Squeezy merges built-in defaults, user settings, project `squeezy.toml`, per-repo user settings, environment variables, and CLI flags. `squeezy config inspect` prints the effective merged configuration with sensitive values redacted, and `squeezy --health` validates the configuration and prints the source chain.",
        docs: &["docs/CONFIGURATION.md", "docs/REPO_PROFILE.md"],
        config: &[
            "model",
            "session",
            "permissions",
            "telemetry",
            "feedback",
            "redaction",
            "web",
            "skills",
            "graph",
            "cache",
            "tui",
        ],
    },
    TopicDefinition {
        id: "providers",
        title: "providers, models, and API key environment names",
        aliases: &[
            "provider",
            "providers",
            "model",
            "models",
            "openai",
            "anthropic",
            "google",
            "gemini",
            "azure",
            "ollama",
            "bedrock",
            "api key",
            "api-key",
        ],
        summary: "Provider selection is configuration-driven. Squeezy supports built-in OpenAI, Anthropic, Google Gemini, Azure OpenAI, Ollama, and Bedrock provider metadata. API key settings name environment variables; `config inspect` redacts secret-looking values and provider key names where appropriate.",
        docs: &["docs/PROVIDERS.md", "docs/CONFIGURATION.md"],
        config: &["model", "providers.*"],
    },
    TopicDefinition {
        id: "permissions",
        title: "permissions, approvals, and shell sandboxing",
        aliases: &[
            "permission",
            "permissions",
            "approval",
            "approvals",
            "policy",
            "sandbox",
            "shell sandbox",
            "allow",
            "ask",
            "deny",
        ],
        summary: "Squeezy separates permission policy from OS shell sandboxing. Permissions decide whether read, edit, shell, web, and MCP operations may start. The shell sandbox is defense in depth for approved commands and can run in required, best-effort, or off modes.",
        docs: &["docs/SHELL_SANDBOXING.md", "docs/CONFIGURATION.md"],
        config: &["permissions", "permissions.shell_sandbox"],
    },
    TopicDefinition {
        id: "skills",
        title: "local skills and built-in Squeezy help",
        aliases: &[
            "skill",
            "skills",
            "/skill",
            "trigger",
            "triggers",
            "help",
            "/help",
            "squeezy help",
        ],
        summary: "User and project skills are local `SKILL.md` directories that inject specialized instructions only when activated. Built-in Squeezy help is separate: `/help <topic>` answers product questions from the bundled docs and config inspect output without granting tools or changing permissions.",
        docs: &["docs/SKILLS.md"],
        config: &["skills"],
    },
    TopicDefinition {
        id: "sessions",
        title: "sessions, logs, resume, and transcript export",
        aliases: &[
            "session",
            "sessions",
            "resume",
            "transcript",
            "logs",
            "session logs",
            "session-export",
        ],
        summary: "Squeezy stores local session logs when configured, including resumable conversation state, redacted events, cost metrics, and session metadata. The TUI has slash commands for listing, showing, resuming, exporting, reporting, and cleaning up sessions.",
        docs: &["docs/SESSIONS.md"],
        config: &["session"],
    },
    TopicDefinition {
        id: "feedback",
        title: "feedback, reports, redaction, and privacy",
        aliases: &[
            "feedback",
            "report",
            "bug report",
            "bug-report",
            "redaction",
            "redact",
            "privacy",
            "secret",
            "secrets",
        ],
        summary: "Feedback and bug reports are consented support paths. Squeezy prepares redacted previews before sending, keeps report archives bounded, and uses the redaction policy to scrub known secret formats and configured custom patterns.",
        docs: &["docs/FEEDBACK.md", "docs/CONFIGURATION.md"],
        config: &["feedback", "redaction"],
    },
    TopicDefinition {
        id: "telemetry",
        title: "anonymous product telemetry",
        aliases: &[
            "telemetry",
            "analytics",
            "opt out",
            "opt-out",
            "product observability",
        ],
        summary: "Squeezy telemetry is anonymous product observability. It records runtime-level events and aggregate metrics, not prompts, completions, file contents, commands, URLs, repository names, paths, or environment values. It can be disabled in configuration.",
        docs: &["docs/TELEMETRY.md", "docs/CONFIGURATION.md"],
        config: &["telemetry"],
    },
    TopicDefinition {
        id: "navigation",
        title: "semantic navigation and language coverage",
        aliases: &[
            "semantic",
            "graph",
            "navigation",
            "declaration",
            "declarations",
            "reference",
            "references",
            "hierarchy",
            "languages",
            "rust",
            "python",
            "javascript",
            "typescript",
            "java",
            "go",
            "c++",
            "unsupported language",
        ],
        summary: "Squeezy uses tree-sitter backed semantic graph operations for declarations, references, hierarchy, flow, dependency paths, impact, and exact read slices. Unsupported languages fall back to ordinary bounded tools and must not fabricate graph confidence.",
        docs: &[
            "docs/SEMANTIC_GRAPH.md",
            "docs/LANGUAGES.md",
            "docs/README.md",
        ],
        config: &["graph"],
    },
    TopicDefinition {
        id: "checkpoints",
        title: "checkpoints, undo, and revert",
        aliases: &["checkpoint", "checkpoints", "undo", "revert", "revert-turn"],
        summary: "Checkpoints preserve local before and after trees for agent edits. TUI commands expose checkpoint listing, checkpoint detail, undo of the latest checkpoint, and turn-level revert through the checkpoint tools.",
        docs: &["docs/CHECKPOINTS.md"],
        config: &[],
    },
    TopicDefinition {
        id: "cost",
        title: "cost controls, receipts, and tool output budgets",
        aliases: &[
            "cost",
            "costs",
            "budget",
            "budgets",
            "receipt",
            "receipts",
            "token",
            "tokens",
            "tool output",
            "spill",
            "cache",
            "dedupe",
        ],
        summary: "Squeezy treats model context as a budgeted resource. The runtime uses capped search/read tools, compact previews for large outputs, receipt stubs for repeated reads, aggregate result budgets, and prompt/cache accounting to reduce repeated context spend.",
        docs: &["docs/tool-call-saving-strategy.md", "docs/CONFIGURATION.md"],
        config: &["budgets", "cache"],
    },
    TopicDefinition {
        id: "mcp-web",
        title: "MCP servers and external web lookup",
        aliases: &[
            "mcp",
            "web",
            "websearch",
            "webfetch",
            "exa",
            "external docs",
            "public docs",
            "lookup",
        ],
        summary: "Squeezy can configure MCP servers and permission-gated web tools, but built-in Squeezy help does not fetch the network automatically. External lookup belongs to explicit web or docs tooling when current public information is needed.",
        docs: &["docs/CONFIGURATION.md", "README.md"],
        config: &["web", "mcp.servers.*"],
    },
    TopicDefinition {
        id: "health",
        title: "health checks, platforms, and startup mode",
        aliases: &[
            "health",
            "--health",
            "platform",
            "platforms",
            "macos",
            "linux",
            "install",
            "startup",
            "mode",
            "/plan",
            "/build",
        ],
        summary: "`squeezy --health` validates configuration without opening the TUI. The first supported platforms are macOS and Linux, and the TUI can start in build or plan mode through config or `--mode plan|build`; inside the TUI, `/plan` and `/build` switch session mode.",
        docs: &["docs/PLATFORMS.md", "docs/CONFIGURATION.md"],
        config: &["session", "tui"],
    },
];

// Documentation paths cited in help answers. These are intentionally *not*
// embedded into the binary via `include_str!`; the topic bodies use curated
// `summary` strings and the renderer only emits the path as a citation, so
// shipping the full corpus would add ~95 KB of unused bytes per CLI build.
// Presence of each file is verified by a unit test in `lib_tests.rs` so a
// renamed or deleted doc still fails CI.
const BUNDLED_DOC_PATHS: &[&str] = &[
    "README.md",
    "docs/CHECKPOINTS.md",
    "docs/CONFIGURATION.md",
    "docs/FEEDBACK.md",
    "docs/LANGUAGES.md",
    "docs/PLATFORMS.md",
    "docs/PROVIDERS.md",
    "docs/REPO_PROFILE.md",
    "docs/README.md",
    "docs/SEMANTIC_GRAPH.md",
    "docs/SESSIONS.md",
    "docs/SHELL_SANDBOXING.md",
    "docs/SKILLS.md",
    "docs/TELEMETRY.md",
    "docs/tool-call-saving-strategy.md",
];

fn parse_help_command(input: &str) -> Option<&str> {
    let rest = input.strip_prefix("/help")?;
    if rest.is_empty() {
        return Some("");
    }
    if !rest.chars().next().is_some_and(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

fn looks_like_squeezy_help_question(input: &str) -> bool {
    // `raw` keeps slashes and dashes so we can match the slash-command markers
    // (`/help`, `/plan`, `--health`, ...) verbatim. `lowered` strips them via
    // `normalize` so the natural-language checks work on plain word tokens.
    let raw = input.trim().to_ascii_lowercase();
    let lowered = normalize(input);
    if lowered.is_empty() {
        return false;
    }
    let product_marker = contains_any(&lowered, &["squeezy", "squeezyagent", "config inspect"])
        || contains_any(
            &raw,
            &[
                "--health",
                "/skill",
                "/help",
                "/feedback",
                "/report",
                "/session",
                "/sessions",
                "/plan",
                "/build",
            ],
        );
    if !product_marker {
        return false;
    }
    // Implementation/debugging requests that happen to mention "squeezy" are
    // coding work, not product-help questions. Bail out so the model handles them
    // instead of returning a canned topic summary.
    if contains_implementation_verb(&lowered) {
        return false;
    }
    lowered.ends_with('?')
        || starts_with_any(
            &lowered,
            &[
                "how ", "what ", "where ", "why ", "when ", "can ", "does ", "do ", "is ", "are ",
                "show ", "list ", "tell ", "explain ",
            ],
        )
        || contains_any(
            &lowered,
            &[
                "how do i",
                "how can i",
                "what is",
                "where is",
                "show me",
                "tell me",
                "explain",
            ],
        )
}

fn contains_implementation_verb(lowered: &str) -> bool {
    // Word forms (already lower-cased; `normalize` collapsed `-` and `_` to spaces).
    // Kept as exact word matches so common nouns like `address`, `addition`, or
    // `fixture` do not accidentally trip the gate.
    const VERB_WORDS: &[&str] = &[
        "implement",
        "implements",
        "implementing",
        "implementation",
        "refactor",
        "refactors",
        "refactoring",
        "refactored",
        "debug",
        "debugs",
        "debugging",
        "debugged",
        "port",
        "porting",
        "ported",
        "add",
        "adds",
        "adding",
        "added",
        "fix",
        "fixes",
        "fixing",
        "fixed",
        "write",
        "writes",
        "writing",
        "wrote",
        "written",
        "create",
        "creates",
        "creating",
        "created",
        "modify",
        "modifies",
        "modifying",
        "modified",
    ];
    lowered.split_whitespace().any(|word| {
        let trimmed = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        VERB_WORDS.contains(&trimmed)
    })
}

fn find_topic(input: &str) -> Option<&'static TopicDefinition> {
    let normalized = normalize(input);
    TOPICS.iter().find(|topic| {
        topic.id == normalized
            || topic
                .aliases
                .iter()
                .any(|alias| normalize(alias) == normalized)
    })
}

fn best_topic_for_text(input: &str) -> Option<&'static TopicDefinition> {
    let normalized = normalize(input);
    let mut best = None;
    let mut best_score = 0;
    for topic in TOPICS {
        let mut score = 0;
        if normalized.contains(topic.id) {
            score += 3;
        }
        for alias in topic.aliases {
            if normalized.contains(&normalize(alias)) {
                score += alias.split_whitespace().count().max(1);
            }
        }
        if score > best_score {
            best_score = score;
            best = Some(topic);
        }
    }
    best
}

fn extract_config_sections(config_inspect: &str, wanted: &[&str]) -> Vec<ConfigSection> {
    if wanted.is_empty() {
        return Vec::new();
    }
    let parsed = parse_config_sections(config_inspect);
    let mut seen = BTreeSet::new();
    let mut sections = Vec::new();
    for pattern in wanted {
        if let Some(prefix) = pattern.strip_suffix(".*") {
            // Require a dot separator so `providers.*` matches `[providers.openai]`
            // but not `[providers_extra]` or `[providersanything]`. The parent
            // section name itself (`[providers]`) is also accepted.
            let dotted = format!("{prefix}.");
            for section in parsed
                .iter()
                .filter(|section| section.name == prefix || section.name.starts_with(&dotted))
            {
                if seen.insert(section.name.clone()) {
                    sections.push(section.clone());
                }
            }
            continue;
        }
        for section in parsed.iter().filter(|section| section.name == *pattern) {
            if seen.insert(section.name.clone()) {
                sections.push(section.clone());
            }
        }
    }
    sections
}

fn parse_config_sections(config_inspect: &str) -> Vec<ConfigSection> {
    let mut sections = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current = String::new();
    for line in config_inspect.lines() {
        if let Some(name) = parse_section_header(line)
            && let Some(previous) = current_name.replace(name.to_string())
        {
            sections.push(ConfigSection {
                name: previous,
                content: current.trim_end().to_string(),
            });
            current.clear();
        }
        if current_name.is_some() {
            current.push_str(line);
            current.push('\n');
        }
    }
    if let Some(name) = current_name {
        sections.push(ConfigSection {
            name,
            content: current.trim_end().to_string(),
        });
    }
    sections
}

fn parse_section_header(line: &str) -> Option<&str> {
    // Only recognises bare `[section.name]` headers as emitted by
    // `inspect_redacted`. Array-of-tables headers (`[[...]]`) are rejected so
    // their contents are skipped, and quoted keys like `["foo bar"]` are not
    // handled here because `inspect_redacted` never emits them today; future
    // config additions that introduce quoted keys must extend this parser.
    let trimmed = line.trim();
    let inner = trimmed.strip_prefix('[')?.strip_suffix(']')?;
    if inner.starts_with('[') || inner.ends_with(']') {
        return None;
    }
    Some(inner)
}

fn normalize(input: &str) -> String {
    input
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn starts_with_any(haystack: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| haystack.starts_with(prefix))
}

pub fn bundled_doc_paths() -> Vec<&'static str> {
    BUNDLED_DOC_PATHS.to_vec()
}

/// Cheap predicate that returns true when [`SqueezyHelp::answer_for_input`] would
/// produce an answer for `input`. Lets callers (e.g. the agent) skip the cost of
/// rendering a redacted config snapshot on turns where the help intercept does
/// not apply.
pub fn matches_squeezy_help_input(input: &str) -> bool {
    let trimmed = input.trim();
    parse_help_command(trimmed).is_some() || looks_like_squeezy_help_question(trimmed)
}
