use std::{collections::BTreeSet, fs, path::Path};

use super::{
    HelpAnswerSource, HelpCitation, HelpStatus, SqueezyHelp, bundled_doc_paths, bundled_docs,
    extract_doc_intro, matches_squeezy_help_input, relevant_docs_for_input,
};

#[test]
fn squeezy_help_config_answer_cites_docs_and_config_sections() {
    let help = SqueezyHelp::new(
        r#"[model]
provider = "openai"
model = "gpt-test"

[providers.openai]
api_key_env = "<redacted>"
base_url = "https://api.openai.com/v1"

[skills]
user_dir = "/tmp/skills"
compat_user_dir = "/tmp/agent-skills"
"#,
    );

    let answer = help.answer_topic("providers");

    assert_eq!(answer.status, HelpStatus::Answered);
    assert!(answer.config_sections.contains(&"model".to_string()));
    assert!(
        answer
            .config_sections
            .contains(&"providers.openai".to_string())
    );
    assert!(answer.citations.contains(&HelpCitation::DocsPath(
        "docs/external/PROVIDERS.md".to_string()
    )));
    assert!(
        answer
            .citations
            .contains(&HelpCitation::ConfigInspectSection("model".to_string()))
    );
    let rendered = answer.render_markdown();
    assert!(rendered.contains("[providers.openai]"), "{rendered}");
    assert!(!rendered.contains("--api-key"), "{rendered}");
    assert!(!rendered.contains("[providers.fake]"), "{rendered}");
}

#[test]
fn squeezy_help_refuses_unsupported_self_questions_with_public_pointers() {
    let help = SqueezyHelp::new("");
    let answer = help
        .answer_for_input("/help quantum_billing")
        .expect("explicit /help command should always produce an answer");

    assert_eq!(answer.status, HelpStatus::Unsupported);
    let rendered = answer.render_markdown();
    assert!(
        rendered.contains("No local help coverage for"),
        "{rendered}"
    );
    assert!(
        rendered.contains("https://squeezyagent.com/docs/"),
        "{rendered}"
    );
    assert!(
        rendered.contains("https://github.com/esqueezy/squeezy"),
        "{rendered}"
    );
}

#[test]
fn squeezy_help_falls_through_when_no_curated_topic_matches() {
    let help = SqueezyHelp::new("");
    // A natural-language Squeezy-self question that no curated topic answers
    // must fall through to the model loop, not produce an `Unsupported` dump.
    assert!(
        help.answer_for_input("Does Squeezy support quantum billing?")
            .is_none(),
        "natural-language prompts without a curated topic must reach the model"
    );
    assert!(
        !matches_squeezy_help_input("Does Squeezy support quantum billing?"),
        "matches_squeezy_help_input must agree"
    );
}

#[test]
fn squeezy_help_ignores_unrelated_questions() {
    let help = SqueezyHelp::new("");

    assert!(
        help.answer_for_input("How do I configure serde?").is_none(),
        "unrelated coding questions should stay on the model path"
    );
    assert!(
        help.answer_for_input("help me implement Squeezy features")
            .is_none(),
        "implementation requests should not be captured by product help"
    );
}

#[test]
fn squeezy_help_ignores_implementation_and_debugging_requests() {
    let help = SqueezyHelp::new("");
    let cases = [
        "How do I implement a new provider in Squeezy?",
        "refactor the squeezy graph crate",
        "debug squeezy cache eviction",
        "Add a new MCP server config to Squeezy",
        "Can you fix the squeezy --health crash?",
        "Please write a new squeezy skill for me",
    ];
    for input in cases {
        assert!(
            help.answer_for_input(input).is_none(),
            "intercept must not capture implementation request: {input}"
        );
        assert!(
            !matches_squeezy_help_input(input),
            "matches_squeezy_help_input must agree: {input}"
        );
    }
}

#[test]
fn matches_squeezy_help_input_agrees_with_answer_for_input() {
    let help = SqueezyHelp::new("");
    let positives = [
        "/help",
        "/help providers",
        "/help quantum_billing",
        "How do I configure Squeezy providers?",
    ];
    for input in positives {
        assert!(
            matches_squeezy_help_input(input),
            "matches_squeezy_help_input should accept: {input}"
        );
        assert!(
            help.answer_for_input(input).is_some(),
            "answer_for_input should accept: {input}"
        );
    }
    let negatives = [
        "How do I configure serde?",
        "build a new tool",
        // Natural-language Squeezy question that no curated topic answers must
        // fall through to the model, not produce a canned `Unsupported` dump.
        "Does Squeezy support quantum billing?",
    ];
    for input in negatives {
        assert!(
            !matches_squeezy_help_input(input),
            "matches_squeezy_help_input should reject: {input}"
        );
        assert!(
            help.answer_for_input(input).is_none(),
            "answer_for_input should reject: {input}"
        );
    }
}

#[test]
fn squeezy_help_ignores_code_navigation_prompts() {
    let help = SqueezyHelp::new("");
    let cases = [
        "where does Agent::start_turn live in Squeezy?",
        "How does the SqueezyAgent struct route turns in Squeezy?",
        "what does start_turn do in squeezy?",
        "Where in squeezy is `compile_exploration_plan` defined?",
        "find squeezy_agent.rs",
    ];
    for input in cases {
        assert!(
            help.answer_for_input(input).is_none(),
            "code-navigation prompt must reach the model: {input}"
        );
        assert!(
            !matches_squeezy_help_input(input),
            "matches_squeezy_help_input must agree: {input}"
        );
    }
}

#[test]
fn squeezy_help_alias_routes_to_providers_topic() {
    let help = SqueezyHelp::new("");
    let answer = help.answer_for_input("/help model").expect("alias answer");
    assert_eq!(answer.status, HelpStatus::Answered);
    assert_eq!(answer.topic, "providers");
}

#[test]
fn squeezy_help_free_text_routes_to_curated_topic() {
    // `/help <free text>` that does not exactly match a topic id/alias should
    // still resolve locally via word-boundary scoring instead of falling
    // through to the slow, provider-backed DocHelp subagent.
    let help = SqueezyHelp::new("");
    let answer = help
        .answer_for_input("/help how to change the model")
        .expect("free-text /help answer");
    assert_eq!(
        answer.status,
        HelpStatus::Answered,
        "free-text /help should resolve to a curated topic, not Unsupported"
    );
    assert_eq!(answer.topic, "providers");
    assert_eq!(answer.source, HelpAnswerSource::LocalCurated);
}

#[test]
fn squeezy_help_off_topic_query_stays_unsupported() {
    // A genuinely off-topic `/help` argument has no word-boundary hit, so the
    // fuzzy fallback must not fabricate a curated answer.
    let help = SqueezyHelp::new("");
    let answer = help
        .answer_for_input("/help zzzzqqq wuxyzzy plover")
        .expect("unsupported answer");
    assert_eq!(answer.status, HelpStatus::Unsupported);
}

#[test]
fn squeezy_help_unknown_slash_arg_keeps_slash_suggestions() {
    // A `/help /unknowncmd <word>` must not be hijacked by a stray word that
    // matches a topic alias (`model`); slash-prefixed args keep the exact-only
    // path so `unsupported` can surface slash-command suggestions instead.
    let help = SqueezyHelp::new("");
    let answer = help
        .answer_for_input("/help /notacommand model")
        .expect("unsupported answer");
    assert_eq!(answer.status, HelpStatus::Unsupported);
    assert_ne!(answer.topic, "providers");
}

#[test]
fn squeezy_help_routes_agent_approach_and_tool_questions() {
    let help = SqueezyHelp::new("");

    let approach = help
        .answer_for_input("How does Squeezy work?")
        .expect("approach answer");
    assert_eq!(approach.status, HelpStatus::Answered);
    assert_eq!(approach.topic, "agent");
    assert!(approach.citations.contains(&HelpCitation::DocsPath(
        "docs/external/AGENT_APPROACH.md".to_string()
    )));

    let tools = help
        .answer_for_input("What tools does Squeezy have?")
        .expect("tools answer");
    assert_eq!(tools.status, HelpStatus::Answered);
    assert_eq!(tools.topic, "agent");
    assert!(tools.citations.contains(&HelpCitation::DocsPath(
        "docs/external/TOOLS.md".to_string()
    )));
}

#[test]
fn squeezy_help_routes_cancel_questions_to_cancel_topic() {
    let help = SqueezyHelp::new("");

    let cancel_turn = help
        .answer_for_input("how do I cancel a squeezy turn?")
        .expect("cancel turn answer");
    assert_eq!(cancel_turn.status, HelpStatus::Answered);
    assert_eq!(cancel_turn.topic, "cancel");
    let body = cancel_turn.render_markdown();
    assert!(body.contains("Esc"), "cancel topic must name Esc: {body}");
    assert!(
        body.contains("Ctrl+C") || body.contains("Ctrl-C"),
        "cancel topic must name Ctrl+C: {body}"
    );

    // The canonical eval prompt that previously hijacked into the agent
    // topic must now route to the cancel topic.
    let in_flight = help
        .answer_for_input(
            "How do I cancel an in-flight model response in squeezy? \
             Answer in one short sentence — name the key or command a user would press.",
        )
        .expect("in-flight cancel answer");
    assert_eq!(in_flight.topic, "cancel");
}

#[test]
fn squeezy_help_falls_through_for_wild_squeezy_questions() {
    let help = SqueezyHelp::new("");
    // Wild "how do I ... squeezy?" questions that no curated topic answers
    // must return None so the model loop handles them, instead of dumping
    // a generic topic + redacted config block.
    let wild = [
        "How do I make squeezy whistle?",
        "How do I teach squeezy to bake bread?",
    ];
    for input in wild {
        assert!(
            help.answer_for_input(input).is_none(),
            "wild squeezy prompt must reach the model: {input}"
        );
        assert!(
            !matches_squeezy_help_input(input),
            "matches_squeezy_help_input must agree: {input}"
        );
    }
}

#[test]
fn extract_config_sections_wildcard_does_not_match_unrelated_prefix() {
    let inspect = r#"[providers.openai]
api_key_env = "<redacted>"

[providers.anthropic]
api_key_env = "<redacted>"

[providers_extra]
note = "should not be selected by providers.* wildcard"
"#;
    let help = SqueezyHelp::new(inspect);
    let answer = help.answer_topic("providers");

    assert!(
        answer
            .config_sections
            .iter()
            .any(|name| name == "providers.openai"),
        "expected providers.openai, got {:?}",
        answer.config_sections
    );
    assert!(
        answer
            .config_sections
            .iter()
            .any(|name| name == "providers.anthropic"),
        "expected providers.anthropic, got {:?}",
        answer.config_sections
    );
    assert!(
        answer
            .config_sections
            .iter()
            .all(|name| name != "providers_extra"),
        "providers.* must not match providers_extra: {:?}",
        answer.config_sections
    );
    let rendered = answer.render_markdown();
    assert!(
        !rendered.contains("[providers_extra]"),
        "rendered output should not include providers_extra: {rendered}"
    );
}

#[test]
fn extract_config_sections_skips_array_of_tables_headers() {
    // `[[providers.openai]]` must not be parsed as a section name; otherwise
    // the array-of-tables body could leak into help answers untouched by the
    // wildcard filter.
    let inspect = r#"[[providers.openai]]
api_key_env = "<redacted>"
secret = "sk-should-not-leak"

[providers.anthropic]
api_key_env = "<redacted>"
"#;
    let help = SqueezyHelp::new(inspect);
    let answer = help.answer_topic("providers");

    assert!(
        answer
            .config_sections
            .iter()
            .all(|name| name != "providers.openai"),
        "array-of-tables header must not register as providers.openai: {:?}",
        answer.config_sections
    );
    let rendered = answer.render_markdown();
    assert!(
        !rendered.contains("sk-should-not-leak"),
        "array-of-tables body must not leak into rendered help: {rendered}"
    );
}

#[test]
fn bundled_doc_paths_exist_on_disk() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let docs_dir = manifest_dir.join("external-docs");
    for path in bundled_doc_paths() {
        let file_name: &str = path
            .rsplit('/')
            .next()
            .expect("bundled doc path has filename");
        let full = docs_dir.join(file_name);
        assert!(
            full.is_file(),
            "bundled doc {path} should exist at {}",
            full.display()
        );
    }
}

#[test]
fn bundled_docs_are_complete_external_corpus() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let docs_dir = manifest_dir.join("external-docs");
    let bundled = bundled_docs();
    let bundled_paths = bundled.iter().map(|doc| doc.path).collect::<BTreeSet<_>>();

    for doc in &bundled {
        assert!(
            doc.path.starts_with("docs/external/"),
            "bundled help doc must be external: {}",
            doc.path
        );
        assert!(
            !doc.content.trim().is_empty(),
            "bundled help doc should embed content: {}",
            doc.path
        );
    }

    for entry in fs::read_dir(&docs_dir).expect("read external-docs") {
        let entry = entry.expect("external doc entry");
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let file_name = path
            .file_name()
            .expect("doc filename")
            .to_string_lossy()
            .into_owned();
        let logical = format!("docs/external/{file_name}");
        assert!(
            bundled_paths.contains(logical.as_str()),
            "external doc should be bundled for help: {logical}"
        );
    }
}

#[test]
fn slash_help_theme_answers_locally() {
    let help = SqueezyHelp::new("");
    let answer = help.answer_for_input("/help /theme").expect("theme answer");
    assert_eq!(answer.status, HelpStatus::Answered);
    assert_eq!(answer.topic, "/theme");
    let body = answer.render_markdown();
    assert!(body.contains("## /theme"), "{body}");
    assert!(body.contains("Syntax:"), "{body}");
    assert!(body.contains("catppuccin"), "{body}");
    assert!(body.contains("high-contrast"), "{body}");
    assert!(body.contains("Requires: [edit]"), "{body}");
}

#[test]
fn slash_help_router_answers_locally() {
    let help = SqueezyHelp::new("");
    let answer = help
        .answer_for_input("/help /router")
        .expect("router answer");
    assert_eq!(answer.status, HelpStatus::Answered);
    assert_eq!(answer.topic, "/router");
    let body = answer.render_markdown();
    assert!(body.contains("routing"), "{body}");
}

#[test]
fn slash_help_unknown_command_suggests_closest() {
    let help = SqueezyHelp::new("");
    let answer = help
        .answer_for_input("/help /them")
        .expect("answer for unknown command");
    let body = answer.render_markdown();
    assert!(
        body.contains("/theme"),
        "should suggest /theme for /them: {body}"
    );
}

#[test]
fn slash_help_index_shows_grouped_topics() {
    let help = SqueezyHelp::new("");
    let answer = help.answer_for_input("/help").expect("index answer");
    let body = answer.render_markdown();
    assert!(body.contains("Getting started"), "{body}");
    assert!(body.contains("Navigation"), "{body}");
}

#[test]
fn slash_command_help_table_has_no_duplicate_names() {
    // The drift tests dedup names into a HashSet and so cannot catch a
    // duplicate `name` shadowing a sibling entry. Assert uniqueness here so a
    // second source-of-truth description for one command fails loudly.
    let mut seen = BTreeSet::new();
    for name in super::slash_command_help_names() {
        assert!(
            seen.insert(name),
            "duplicate slash-command help entry for {name:?}"
        );
    }
}

#[test]
fn squeezy_help_doc_citations_are_bundled_paths() {
    let bundled = bundled_doc_paths().into_iter().collect::<BTreeSet<_>>();
    let topics = [
        "agent",
        "tui",
        "config",
        "providers",
        "permissions",
        "skills",
        "sessions",
        "feedback",
        "telemetry",
        "navigation",
        "checkpoints",
        "cost",
        "mcp-web",
        "install",
        "hooks",
        "prompt-templates",
        "health",
    ];
    let help = SqueezyHelp::new("");

    for topic in topics {
        let answer = help.answer_topic(topic);
        for citation in answer.citations {
            if let HelpCitation::DocsPath(path) = citation {
                assert!(bundled.contains(path.as_str()), "missing {path}");
            }
        }
    }
}

#[test]
fn relevant_docs_for_input_scopes_corpus() {
    // Known topic (/help providers): must include PROVIDERS.md, must NOT include SESSIONS.md.
    let providers_docs = relevant_docs_for_input("/help providers");
    let providers_paths: Vec<&str> = providers_docs.iter().map(|d| d.path).collect();
    assert!(
        providers_paths.contains(&"docs/external/PROVIDERS.md"),
        "providers corpus must include PROVIDERS.md: {providers_paths:?}"
    );
    assert!(
        !providers_paths.contains(&"docs/external/SESSIONS.md"),
        "providers corpus must NOT include unrelated SESSIONS.md: {providers_paths:?}"
    );
    assert!(
        providers_docs.len() < bundled_docs().len(),
        "providers corpus ({}) must be smaller than full corpus ({})",
        providers_docs.len(),
        bundled_docs().len()
    );

    // Completely unknown topic (no lexical evidence) falls back to full corpus.
    // Use terms that are guaranteed not to appear in any bundled doc so the
    // zero-score fallback path is exercised.
    let unknown_docs = relevant_docs_for_input("/help xylophone kazoo fluorescent");
    assert_eq!(
        unknown_docs.len(),
        bundled_docs().len(),
        "truly unknown-topic corpus must be the full corpus so DocHelp has maximum coverage: \
         got {} docs, full corpus is {}",
        unknown_docs.len(),
        bundled_docs().len()
    );
    assert!(
        unknown_docs
            .iter()
            .any(|d| d.path == "docs/external/AGENT_APPROACH.md"),
        "full corpus must contain AGENT_APPROACH.md"
    );

    // Lexically-matchable unknown topic (e.g. mentions "provider" or "model"
    // keywords that appear in docs) should return fewer than the full corpus.
    // This tests the top-K lexical scorer path.
    let partial_docs = relevant_docs_for_input("/help provider model configuration");
    assert!(
        partial_docs
            .iter()
            .any(|d| d.path == "docs/external/README.md"),
        "lexical fallback must always include anchor README.md"
    );
    assert!(
        partial_docs
            .iter()
            .any(|d| d.path == "docs/external/AGENT_APPROACH.md"),
        "lexical fallback must always include anchor AGENT_APPROACH.md"
    );
    assert!(
        partial_docs.len() < bundled_docs().len(),
        "lexical fallback for a topic with keyword evidence ({} docs) must be smaller than the \
         full corpus ({} docs) — regression: scorer is returning the full corpus",
        partial_docs.len(),
        bundled_docs().len()
    );
}

#[test]
fn extract_doc_intro_does_not_panic_on_multibyte_boundary() {
    // First paragraph longer than the char cap, with a 3-byte em-dash placed so
    // that it straddles the byte offset a naive `&s[..max_chars]` slice would
    // land on. Regression for the byte-vs-char slice panic.
    let mut para = "a".repeat(399);
    para.push('—'); // U+2014, 3 bytes: occupies char index 399 (the cap boundary)
    para.push_str(&"b".repeat(50));
    let content = format!("# Heading\n\n{para}\n\nsecond paragraph");

    // Must not panic and must stay on char boundaries (i.e. be valid &str).
    let intro = extract_doc_intro(&content, 400);

    // Capped at exactly `max_chars` characters from the first paragraph.
    assert_eq!(intro.chars().count(), 400);
    assert!(intro.starts_with('a'));
    // The em-dash is the 400th char, so it must be fully included (not split).
    assert!(intro.ends_with('—'));
    // First-paragraph semantics: never bleeds into the second paragraph.
    assert!(!intro.contains("second paragraph"));
}

#[test]
fn extract_doc_intro_returns_short_paragraph_unchanged() {
    let content = "# Heading\n\nshort intro\n\nmore text";
    assert_eq!(extract_doc_intro(content, 400), "short intro");
}
