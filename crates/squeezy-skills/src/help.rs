use std::{collections::BTreeSet, fmt::Write as _};

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
            output.push_str("\n\n**Sources:**\n");
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
            Self::DocsPath(path) => format!("See: {path}"),
            Self::ConfigInspectSection(section) => {
                format!("From config: [{section}]")
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
        let mut topics = String::new();
        for topic in TOPICS {
            if !topics.is_empty() {
                topics.push('\n');
            }
            let _ = write!(topics, "- `{}`: {}", topic.id, topic.title);
        }
        HelpAnswer {
            topic: "index".to_string(),
            status: HelpStatus::Answered,
            body: format!(
                "Available `/help` topics:\n{topics}\n\nUse `/help <topic>` for a local answer grounded in bundled docs. For broader coverage: https://squeezyagent.com/docs/"
            ),
            citations: vec![
                HelpCitation::DocsPath("docs/external/README.md".to_string()),
                HelpCitation::DocsPath("docs/external/SKILLS.md".to_string()),
                HelpCitation::DocsPath("docs/external/TOOLS.md".to_string()),
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
        // Only intercept when the prompt directly matches a curated topic via
        // word-boundary alias or id hits. If `best_topic_for_text` cannot find
        // a real match, let the prompt fall through to the model loop instead
        // of dumping a generic "agent" topic + redacted config block.
        let topic = best_topic_for_text(trimmed)?;
        Some(self.answer_definition(topic))
    }

    fn answer_definition(&self, definition: &TopicDefinition) -> HelpAnswer {
        let extracted_sections = extract_config_sections(&self.config_inspect, definition.config);
        let mut body = format!("## {}\n\n{}", definition.title, definition.summary);
        if !extracted_sections.is_empty() {
            body.push_str("\n\nRelevant redacted `config inspect` output:\n```toml\n");
            for section in &extracted_sections {
                body.push_str(section.content.trim_end());
                body.push_str("\n\n");
            }
            body.push_str("```");
        }
        // Append a short intro excerpt from each cited doc (first ~400 chars per doc, max 2 docs)
        let mut doc_excerpt_count = 0;
        for path in definition.docs.iter().take(2) {
            if let Some(content) = bundled_doc(path) {
                let excerpt = extract_doc_intro(content, 400);
                if !excerpt.is_empty() {
                    if doc_excerpt_count == 0 {
                        body.push_str("\n\n**From the docs:**\n");
                    }
                    let _ = write!(body, "\n*{}*\n\n{}", path, excerpt);
                    doc_excerpt_count += 1;
                }
            }
        }
        if extracted_sections.is_empty() && doc_excerpt_count == 0 {
            body.push_str(
                "\n\n---\n*Grounded in local Squeezy docs and current config. For newer or broader coverage use `/help` with a different topic, or check [squeezyagent.com/docs](https://squeezyagent.com/docs/).*",
            );
        }

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
        let mut suggestions = String::new();
        for t in TOPICS {
            if !suggestions.is_empty() {
                suggestions.push_str(", ");
            }
            suggestions.push_str(t.id);
        }
        let did_you_mean = {
            let candidates = top_topics_for_text(topic, 3);
            if candidates.is_empty() {
                String::new()
            } else {
                let ids: Vec<&str> = candidates.iter().map(|t| t.id).collect();
                format!("\n\nDid you mean: {}?", ids.join(" or "))
            }
        };
        HelpAnswer {
            topic: topic.trim().to_string(),
            status: HelpStatus::Unsupported,
            body: format!(
                "No local help coverage for `{}`.{did_you_mean}\n\nTry one of these topics: {suggestions}.\n\nFor current or broader documentation: {SQUEEZY_WEBSITE_URL} or {SQUEEZY_REPO_URL}.",
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
        id: "cancel",
        title: "cancel or interrupt an in-flight Squeezy turn",
        aliases: &[
            "cancel turn",
            "cancel a turn",
            "cancel the turn",
            "interrupt",
            "interrupt turn",
            "interrupt a turn",
            "stop turn",
            "stop a turn",
            "abort",
            "abort turn",
            "kill turn",
            "halt",
            "esc",
            "esc key",
            "ctrl c",
            "ctrl+c",
            "keyboard shortcut",
            "key binding",
            "key bindings",
            "footer hint",
        ],
        summary: "Press Esc or Ctrl+C while a Squeezy turn is in flight to cancel it; the same keys also cancel a pending tool approval. The composer footer shows `Ctrl-C/Esc interrupt` while a turn is running, and the request routes through `request_turn_interrupt` in the TUI event loop. There is no `/cancel` or `/stop` slash command today — the cancel surface is keyboard-only.",
        docs: &[
            "docs/external/AGENT_APPROACH.md",
            "docs/external/TROUBLESHOOTING.md",
        ],
        config: &["tui"],
    },
    TopicDefinition {
        id: "tui",
        title: "TUI interface, keyboard shortcuts, and slash commands",
        aliases: &[
            "tui",
            "keyboard",
            "keyboard shortcuts",
            "keybindings",
            "keymap",
            "shortcut",
            "shortcuts",
            "slash command",
            "slash commands",
            "slash menu",
            "composer",
            "footer",
            "status line",
            "statusline",
            "theme",
            "themes",
            "/theme",
            "/router",
            "/reviewer",
            "/statusline",
            "/model",
            "/permissions",
            "/plans",
            "/diff",
            "/keymap",
            "/spinner",
            "/tasks",
            "/verbosity",
            "/effort",
            "/fork",
            "/cheap",
        ],
        summary: "The Squeezy TUI has a full-width composer at the bottom, a transcript pane above it, and a status footer. Key bindings: Esc or Ctrl+C cancel an in-flight turn or tool approval; Enter submits; Ctrl+J inserts a newline; Ctrl+T shows the full transcript overlay; Ctrl+P shows the task-state overlay; Ctrl+Y copies the last assistant message; Ctrl+R restores the last prompt; Shift+Tab toggles plan/build mode; Up/Down navigate input history or the slash menu. Slash commands: `/help`, `/plan`, `/build`, `/router`, `/model`, `/permissions`, `/config`, `/cost`, `/context`, `/compact`, `/pin`, `/unpin`, `/pins`, `/attach`, `/attachments`, `/detach`, `/sessions`, `/session`, `/resume`, `/clear`, `/feedback`, `/report`, `/verbosity`, `/tool-verbosity`, `/tasks`, `/task`, `/task-cancel`, `/skill`, `/theme`, `/reviewer`, `/diff`, `/plans`, `/fork`, `/keymap`, `/spinner`, `/effort`, `/cheap`, `/checkpoints`, `/undo`, `/revert-turn`. Use `/theme <name>` to switch the color theme; built-ins are `default`, `bright`, `fun`, and `starlight`. Use `/router on|off` to toggle cheap-model turn routing. Use `/keymap` to view the active key bindings. Typing any `/` opens the slash suggestion menu; arrow keys navigate it.",
        docs: &["docs/external/AGENT_APPROACH.md", "docs/external/TOOLS.md"],
        config: &["tui", "session"],
    },
    TopicDefinition {
        id: "agent",
        title: "agent approach, modes, tools, and local-first workflow",
        aliases: &[
            "approach",
            "how does squeezy work",
            "how squeezy works",
            "how it works",
            "tool",
            "tools",
            "slash command",
            "slash commands",
            "plan mode",
            "build mode",
            "mode",
            "/plan",
            "/build",
        ],
        summary: "Squeezy is local-first: product-help questions are answered from bundled docs and redacted config, code-navigation questions prefer graph-backed tools before raw file reads, mutating work is gated by plan/build mode and permissions, and large tool outputs are compacted or spilled behind receipts.",
        docs: &[
            "docs/external/AGENT_APPROACH.md",
            "docs/external/TOOLS.md",
            "docs/external/CONFIGURATION.md",
        ],
        config: &["agent", "session", "tools", "budgets", "tui"],
    },
    TopicDefinition {
        id: "config",
        title: "configuration and source precedence",
        aliases: &[
            "configuration",
            "settings",
            "config inspect",
            "squeezy.toml",
            "settings.toml",
        ],
        summary: "Squeezy merges built-in defaults, user settings, project `squeezy.toml`, per-repo user settings, environment variables, and CLI flags. `squeezy config inspect` prints the effective merged configuration with sensitive values redacted, and `squeezy doctor` validates the configuration along with provider credential, session-store, and sandbox checks.",
        docs: &[
            "docs/external/CONFIGURATION.md",
            "docs/external/REPO_PROFILE.md",
        ],
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
        docs: &[
            "docs/external/PROVIDERS.md",
            "docs/external/CONFIGURATION.md",
        ],
        config: &["model", "providers.*"],
    },
    TopicDefinition {
        id: "permissions",
        title: "permissions, approvals, and shell sandboxing",
        aliases: &[
            "permission",
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
        docs: &[
            "docs/external/SHELL_SANDBOXING.md",
            "docs/external/APPROVAL_POLICY.md",
            "docs/external/CONFIGURATION.md",
        ],
        config: &[
            "permissions",
            "permissions.ai_reviewer",
            "permissions.shell_sandbox",
        ],
    },
    TopicDefinition {
        id: "skills",
        title: "local skills and built-in Squeezy help",
        aliases: &[
            "skill",
            "/skill",
            "trigger",
            "triggers",
            "help",
            "/help",
            "squeezy help",
        ],
        summary: "User and project skills are local `SKILL.md` directories that inject specialized instructions only when activated. Built-in Squeezy help is separate: `/help <topic>` answers product questions from the bundled docs and config inspect output without granting tools or changing permissions.",
        docs: &["docs/external/SKILLS.md"],
        config: &["skills"],
    },
    TopicDefinition {
        id: "sessions",
        title: "sessions, logs, resume, and transcript export",
        aliases: &[
            "session",
            "resume",
            "transcript",
            "logs",
            "session logs",
            "session-export",
        ],
        summary: "Squeezy stores local session logs when configured, including resumable conversation state, redacted events, cost metrics, and session metadata. The TUI has slash commands for listing, showing, resuming, exporting, reporting, and cleaning up sessions.",
        docs: &["docs/external/SESSIONS.md"],
        config: &["session"],
    },
    TopicDefinition {
        id: "feedback",
        title: "feedback, reports, redaction, and privacy",
        aliases: &[
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
        docs: &[
            "docs/external/FEEDBACK.md",
            "docs/external/CONFIGURATION.md",
        ],
        config: &["feedback", "redaction"],
    },
    TopicDefinition {
        id: "telemetry",
        title: "anonymous product telemetry",
        aliases: &["analytics", "opt out", "opt-out", "product observability"],
        summary: "Squeezy telemetry is anonymous product observability. It records runtime-level events and aggregate metrics, not prompts, completions, file contents, commands, URLs, repository names, paths, or environment values. It can be disabled in configuration.",
        docs: &[
            "docs/external/TELEMETRY.md",
            "docs/external/CONFIGURATION.md",
        ],
        config: &["telemetry"],
    },
    TopicDefinition {
        id: "navigation",
        title: "semantic navigation and language coverage",
        aliases: &[
            "semantic",
            "graph",
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
            "kotlin",
            "go",
            "c++",
            "c#",
            "csharp",
            ".net",
            "dotnet",
            "php",
            "ruby",
            "scala",
            "swift",
            "dart",
            "unsupported language",
        ],
        summary: "Squeezy uses tree-sitter backed semantic graph operations for declarations, references, hierarchy, flow, dependency paths, impact, and exact read slices. Supported language families: Rust, Python, Java, Kotlin, C#/.NET, Go, C/C++, JavaScript/TypeScript, PHP, Ruby, Scala, Swift, and Dart. Unsupported languages fall back to ordinary bounded tools and must not fabricate graph confidence.",
        docs: &[
            "docs/external/AGENT_APPROACH.md",
            "docs/external/TOOLS.md",
            "docs/external/LANGUAGES.md",
        ],
        config: &["graph"],
    },
    TopicDefinition {
        id: "checkpoints",
        title: "checkpoints, undo, and revert",
        aliases: &["checkpoint", "undo", "revert", "revert-turn"],
        summary: "Checkpointing is disabled by default. When enabled, checkpoints preserve local before and after trees for agent edits, and TUI commands expose listing, detail, undo, and turn-level revert through the checkpoint tools.",
        docs: &["docs/external/CHECKPOINTS.md"],
        config: &["tools.checkpoints_enabled"],
    },
    TopicDefinition {
        id: "cost",
        title: "cost controls, receipts, and tool output budgets",
        aliases: &[
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
        docs: &[
            "docs/external/tool-call-saving-strategy.md",
            "docs/external/AGENT_APPROACH.md",
            "docs/external/CONFIGURATION.md",
        ],
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
        docs: &[
            "docs/external/MCP_AND_WEB.md",
            "docs/external/CONFIGURATION.md",
        ],
        config: &["web", "mcp.servers.*"],
    },
    TopicDefinition {
        id: "install",
        title: "installation, first run, upgrades, and uninstall",
        aliases: &[
            "installation",
            "brew",
            "homebrew",
            "cargo install",
            "github release",
            "release archive",
            "first run",
            "uninstall",
            "upgrade",
        ],
        summary: "Squeezy can be installed with the one-line installer (`curl -fsSL https://raw.githubusercontent.com/esqueezy/squeezy/main/install.sh | sh`), from the `esqueezy/tap` Homebrew tap, with `cargo install squeezy --locked`, or from GitHub release archives for macOS, Linux, and Windows (x86_64). Run `squeezy doctor` after install, initialize user settings with `squeezy config init --user`, and remove the binary plus optional `~/.squeezy` state when uninstalling.",
        docs: &[
            "docs/external/INSTALL.md",
            "docs/external/PLATFORMS.md",
            "docs/external/PROVIDERS.md",
            "docs/external/CONFIGURATION.md",
        ],
        config: &["model", "providers.*", "session"],
    },
    TopicDefinition {
        id: "doctor",
        title: "doctor command, platforms, and startup mode",
        aliases: &[
            "doctor",
            "health",
            "--health",
            "platform",
            "platforms",
            "macos",
            "linux",
            "windows",
            "startup",
            "troubleshooting",
            "troubleshoot",
        ],
        summary: "`squeezy doctor` validates configuration without opening the TUI and reports on the configured provider credential, repo profile, session store, and shell-sandbox availability. Supported platforms are macOS, Linux, and Windows (x86_64). For startup, provider, permission, graph, or local-help issues, run `squeezy doctor` and `squeezy config inspect` first.",
        docs: &[
            "docs/external/PLATFORMS.md",
            "docs/external/TROUBLESHOOTING.md",
            "docs/external/CONFIGURATION.md",
        ],
        config: &["session", "tui"],
    },
    TopicDefinition {
        id: "hooks",
        title: "hook events, mutation points, and skill scripts",
        aliases: &[
            "hook",
            "hooks",
            "hook event",
            "hook events",
            "pre tool",
            "post tool",
            "pre turn",
            "user prompt",
            "permission hook",
            "subagent hook",
            "compaction hook",
            "script",
            "scripts",
            "skill script",
        ],
        summary: "Hooks are observation and mutation points fired at key lifecycle events in the agent loop. They are used internally by skills, telemetry, and MCP integration. Hook scripts are executables placed under a skill's `scripts/` directory; they receive a JSON payload on stdin. Events include PreTurn (may append extra_instructions), UserPromptSubmit (may rewrite the prompt), PreToolUse, PostToolUse, PostToolUseFailure, PreCompact, PostCompact, SubagentStart, SubagentStop, PermissionRequest, PermissionDenied, SessionStart, Stop, and Setup.",
        docs: &["docs/external/HOOKS.md", "docs/external/SKILLS.md"],
        config: &["skills"],
    },
    TopicDefinition {
        id: "prompt-templates",
        title: "prompt templates and reusable slash macros",
        aliases: &[
            "prompt template",
            "prompt templates",
            "prompt macro",
            "/prompt-template",
            "slash macro",
            "prompts dir",
            "custom prompt",
            "reusable prompt",
        ],
        summary: "Prompt templates are reusable `.md` files stored in `~/.squeezy/prompts/` (user scope) or `<workspace>/.squeezy/prompts/` (project scope). Each file's name (without `.md`) becomes the template name. Activate with `/prompt-template <name>` in the composer. Project templates shadow user templates with the same name.",
        docs: &[
            "docs/external/PROMPT_TEMPLATES.md",
            "docs/external/SKILLS.md",
        ],
        config: &["skills"],
    },
];

#[derive(Debug, Clone, Copy)]
pub struct BundledDoc {
    pub path: &'static str,
    pub content: &'static str,
}

// The in-product help corpus is intentionally the external docs directory only.
// Internal implementation, benchmark, and deployment notes stay out of normal
// user help so answers remain user-focused.
include!(concat!(env!("OUT_DIR"), "/bundled_docs.rs"));

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
    let token_slices_for_marker: Vec<String> = lowered
        .split_whitespace()
        .map(|t| clean_token(t).to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let token_refs: Vec<&str> = token_slices_for_marker.iter().map(String::as_str).collect();
    let product_marker = contains_any(&lowered, &["squeezy", "squeezyagent"])
        || contains_word_sequence(&token_refs, "config inspect")
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
    // Code-navigation prompts that name a specific symbol, file, or path are
    // model questions even when they mention "squeezy" (e.g. "where does
    // Agent::start_turn live in squeezy?"). The canned topic summary would
    // hijack a navigation answer.
    if contains_code_navigation_indicator(input) {
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

// Returns true when the user input names a specific code symbol, file path,
// or source token that a navigation answer would be expected to ground on.
// Operates on the raw input (case preserved) so CamelCase detection still works.
fn contains_code_navigation_indicator(input: &str) -> bool {
    const SOURCE_EXTENSIONS: &[&str] = &[
        ".rs", ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".java", ".kt", ".cs", ".cpp", ".cc",
        ".hpp", ".hh", ".c", ".h", ".rb", ".swift", ".scala", ".m", ".mm", ".php", ".sh", ".toml",
        ".yaml", ".yml", ".json", ".sql",
    ];
    let lowered = input.to_ascii_lowercase();
    if input.contains("::") || input.contains("->") || input.contains('`') {
        return true;
    }
    if SOURCE_EXTENSIONS.iter().any(|ext| lowered.contains(ext)) {
        return true;
    }
    for word in input.split_whitespace() {
        let trimmed = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_');
        if trimmed.len() < 3 {
            continue;
        }
        let all_ident_chars = trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !all_ident_chars {
            continue;
        }
        // CamelCase / PascalCase: at least one uppercase AND one lowercase AND
        // more than one uppercase character. Excludes plain acronyms like
        // "API", "MCP", "TUI" while still catching "FooBar", "StartTurn", etc.
        let upper_count = trimmed.chars().filter(|c| c.is_ascii_uppercase()).count();
        let has_lower = trimmed.chars().any(|c| c.is_ascii_lowercase());
        if upper_count >= 2 && has_lower {
            return true;
        }
        // snake_case identifier: at least one underscore between alphanumerics.
        if trimmed.contains('_') {
            return true;
        }
    }
    false
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
    // Match aliases at word boundaries against the normalized prompt so a short
    // alias like `mode` does not score on `model` and `agent` does not score on
    // every sentence that contains a word with that substring. Substring
    // matching is what caused the help intercept to hijack questions like
    // "how do I cancel an in-flight model response in squeezy?" with the
    // generic agent topic dump.
    let normalized = normalize(input);
    let tokens: Vec<String> = normalized
        .split_whitespace()
        .map(|tok| clean_token(tok).to_string())
        .filter(|tok| !tok.is_empty())
        .collect();
    let token_slices: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let mut best = None;
    let mut best_score = 0;
    for topic in TOPICS {
        let mut score = 0;
        if contains_word_sequence(&token_slices, topic.id) {
            score += 3;
        }
        for alias in topic.aliases {
            let alias_norm = normalize(alias);
            if contains_word_sequence(&token_slices, &alias_norm) {
                score += alias_norm.split_whitespace().count().max(1);
            }
        }
        if score > best_score {
            best_score = score;
            best = Some(topic);
        }
    }
    best
}

fn top_topics_for_text(input: &str, n: usize) -> Vec<&'static TopicDefinition> {
    let normalized = normalize(input);
    let tokens: Vec<String> = normalized
        .split_whitespace()
        .map(|tok| clean_token(tok).to_string())
        .filter(|tok| !tok.is_empty())
        .collect();
    let token_slices: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let mut scored: Vec<(usize, &'static TopicDefinition)> = Vec::new();
    for topic in TOPICS {
        let mut score = 0;
        if contains_word_sequence(&token_slices, topic.id) {
            score += 3;
        }
        for alias in topic.aliases {
            let alias_norm = normalize(alias);
            if contains_word_sequence(&token_slices, &alias_norm) {
                score += alias_norm.split_whitespace().count().max(1);
            }
        }
        if score > 0 {
            scored.push((score, topic));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(n).map(|(_, topic)| topic).collect()
}

// Strips trailing/leading sentence punctuation from a token so word-boundary
// matching ignores the `?` on `squeezy?` and the `.` on `press.`. The slash is
// preserved so slash-command aliases like `/plan` still match.
fn clean_token(tok: &str) -> &str {
    tok.trim_matches(|c: char| {
        matches!(
            c,
            '?' | '!' | '.' | ',' | ';' | ':' | '(' | ')' | '[' | ']' | '"' | '\'' | '`' | '—'
        )
    })
}

// Returns true when `needle` (already normalized) appears as a contiguous run
// of whole tokens inside `haystack_tokens`. Empty needles never match.
fn contains_word_sequence(haystack_tokens: &[&str], needle: &str) -> bool {
    let needle_len = needle.split_whitespace().count();
    if needle_len == 0 || needle_len > haystack_tokens.len() {
        return false;
    }
    haystack_tokens
        .windows(needle_len)
        .any(|window| window.iter().copied().eq(needle.split_whitespace()))
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
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    for ch in input.trim().chars() {
        let ch = match ch {
            '_' | '-' => ' ',
            _ => ch.to_ascii_lowercase(),
        };
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn starts_with_any(haystack: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| haystack.starts_with(prefix))
}

pub fn bundled_doc_paths() -> Vec<&'static str> {
    BUNDLED_DOCS.iter().map(|doc| doc.path).collect()
}

pub fn bundled_docs() -> Vec<BundledDoc> {
    BUNDLED_DOCS.to_vec()
}

pub fn bundled_doc(path: &str) -> Option<&'static str> {
    BUNDLED_DOCS
        .iter()
        .find(|doc| doc.path == path)
        .map(|doc| doc.content)
}

pub const APPROVAL_POLICY_DOC_PATH: &str = "docs/external/APPROVAL_POLICY.md";

/// Cheap predicate that returns true when [`SqueezyHelp::answer_for_input`] would
/// produce an answer for `input`. Lets callers (e.g. the agent) skip the cost of
/// rendering a redacted config snapshot on turns where the help intercept does
/// not apply.
pub fn matches_squeezy_help_input(input: &str) -> bool {
    let trimmed = input.trim();
    if parse_help_command(trimmed).is_some() {
        return true;
    }
    looks_like_squeezy_help_question(trimmed) && best_topic_for_text(trimmed).is_some()
}

fn extract_doc_intro(content: &str, max_chars: usize) -> &str {
    let trimmed = content.trim_start();
    // Skip the first heading if present
    let after_heading = if trimmed.starts_with('#') {
        trimmed
            .find('\n')
            .map_or("", |i| &trimmed[i + 1..])
            .trim_start()
    } else {
        trimmed
    };
    // Find end of first paragraph (first blank line = two consecutive newlines)
    let end = after_heading
        .find("\n\n")
        .unwrap_or(after_heading.len())
        .min(max_chars);
    &after_heading[..end]
}

/// Return only the bundled docs that are relevant for the given input, falling
/// back to the full corpus when no curated topic can be identified. Always
/// includes README and AGENT_APPROACH so the subagent has core orientation.
pub fn relevant_docs_for_input(input: &str) -> Vec<BundledDoc> {
    let trimmed = input.trim();
    let topic = parse_help_command(trimmed)
        .filter(|t| !t.is_empty())
        .and_then(find_topic)
        .or_else(|| best_topic_for_text(trimmed));

    let Some(topic) = topic else {
        return bundled_docs();
    };

    let cited: std::collections::HashSet<&str> = topic.docs.iter().copied().collect();
    BUNDLED_DOCS
        .iter()
        .filter(|doc| {
            cited.contains(doc.path)
                || doc.path == "docs/external/README.md"
                || doc.path == "docs/external/AGENT_APPROACH.md"
        })
        .copied()
        .collect()
}

#[cfg(test)]
#[path = "help_tests.rs"]
mod tests;
