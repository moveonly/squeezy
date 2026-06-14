use std::{collections::BTreeSet, fs, path::Path};

use super::{
    BundledDoc, HelpAnswer, HelpAnswerSource, HelpCitation, HelpStatus, SqueezyHelp,
    bundled_doc_paths, bundled_docs, chunk_doc_sections, extract_doc_intro,
    matches_squeezy_help_input, relevant_doc_sections_for_input, relevant_docs_for_input,
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
fn slash_command_help_reports_turn_blocking_and_capabilities() {
    // The help metadata duplicates the slash registry's availability/capability
    // facts across a crate boundary, so it can silently drift. Pin the corrected
    // facts: /tool-verbosity cannot run mid-turn and needs edit; /compact needs
    // net; /effort needs edit — each surfaced through the rendered body.
    let help = SqueezyHelp::new("");

    let tool_verbosity = help
        .answer_for_input("/help /tool-verbosity")
        .expect("answer for /tool-verbosity")
        .render_markdown();
    assert!(
        tool_verbosity.contains("Cannot run while a turn is in progress"),
        "/tool-verbosity must report it cannot run mid-turn: {tool_verbosity}"
    );
    assert!(
        tool_verbosity.contains("**Capability:**") && tool_verbosity.contains("[edit]"),
        "/tool-verbosity must report the edit capability: {tool_verbosity}"
    );

    let compact = help
        .answer_for_input("/help /compact")
        .expect("answer for /compact")
        .render_markdown();
    assert!(
        compact.contains("**Capability:**") && compact.contains("[net]"),
        "/compact must report the net capability: {compact}"
    );

    let effort = help
        .answer_for_input("/help /effort")
        .expect("answer for /effort")
        .render_markdown();
    assert!(
        effort.contains("**Capability:**") && effort.contains("[edit]"),
        "/effort must report the edit capability: {effort}"
    );
}

#[test]
fn tui_topic_summary_does_not_advertise_unregistered_commands() {
    // The `tui` topic summary hand-lists the slash vocabulary and cannot import
    // the live registry across crates, so it drifts. Guard the two failure modes:
    // it must not name commands the registry never had (or hides from the menu),
    // and it must name commands that have since been added.
    let help = SqueezyHelp::new("");
    let body = help.answer_topic("tui").render_markdown();
    for phantom in ["/attachments", "/detach", "/skill"] {
        assert!(
            !body.contains(phantom),
            "tui summary must not advertise {phantom}, which is not in the slash menu: {body}"
        );
    }
    for present in ["/bundle", "/export", "/statusline", "/mcp", "/terminal"] {
        assert!(
            body.contains(present),
            "tui summary must list the registered command {present}: {body}"
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

#[test]
fn revert_turn_help_shows_required_turn_id() {
    // The dispatch parser requires `<turn_id>` (require_id) and errors on a bare
    // `/revert-turn`; the help syntax must teach the argument so the documented
    // form does not immediately fail.
    let help = SqueezyHelp::new("");
    let answer = help
        .answer_for_input("/help /revert-turn")
        .expect("revert-turn answer");
    let body = answer.render_markdown();
    assert!(
        body.contains("<turn_id>"),
        "/revert-turn help must show the required <turn_id>: {body}"
    );
}

#[test]
fn providers_topic_names_gateway_presets_not_just_first_party() {
    // The supported surface is far wider than the curated first-party vendors;
    // the summary must surface at least one OpenAI-compatible gateway so a
    // reader of `/help providers` learns presets like OpenRouter are supported.
    let help = SqueezyHelp::new("");
    let body = help.answer_topic("providers").body;
    assert!(
        body.contains("OpenRouter"),
        "providers summary must mention a gateway preset such as OpenRouter: {body}"
    );
}

#[test]
fn bundled_skill_docs_do_not_advertise_nonexistent_slash_skill() {
    // There is no `/skill` user command in the dispatch parser; skills activate
    // via triggers, the model's `load_skill` tool, or implicit shell use. The
    // bundled docs must not teach a `/skill` invocation that immediately falls
    // through as a literal prompt.
    let targets = [
        "docs/external/SKILLS.md",
        "docs/external/PROMPT_TEMPLATES.md",
    ];
    for doc in bundled_docs() {
        if targets.contains(&doc.path) {
            assert!(
                !doc.content.contains("/skill "),
                "{} must not advertise a `/skill` command",
                doc.path
            );
        }
    }
}

#[test]
fn chunk_doc_sections_splits_on_atx_headings() {
    let doc = BundledDoc {
        path: "docs/external/EXAMPLE.md",
        content: "# Title\n\nintro body\n\n## Alpha\n\nalpha body\n\n### Beta\n\nbeta body\n",
    };
    let sections = chunk_doc_sections(&doc);
    let headings: Vec<&str> = sections.iter().map(|s| s.heading).collect();
    assert_eq!(headings, vec!["# Title", "## Alpha", "### Beta"]);

    // Each section borrows from the doc and includes its heading line + body.
    assert!(sections[0].content.starts_with("# Title"));
    assert!(sections[0].content.contains("intro body"));
    assert!(!sections[0].content.contains("## Alpha"));

    let alpha = sections
        .iter()
        .find(|s| s.heading == "## Alpha")
        .expect("alpha section");
    assert!(alpha.content.contains("alpha body"));
    assert!(!alpha.content.contains("beta body"));
    assert_eq!(alpha.path, "docs/external/EXAMPLE.md");

    // All section content slices borrow from the doc's static content.
    for section in &sections {
        assert!(
            doc.content.contains(section.content),
            "section content must be a subslice of the doc: {:?}",
            section.content
        );
    }
}

#[test]
fn prompt_templates_topic_describes_real_slash_activation() {
    let help = SqueezyHelp::new("");
    let body = help.answer_topic("prompt-templates").body;
    // Templates are invoked as `/<template-name>`, not via a non-existent
    // `/prompt-template` command. The help text must teach the form the
    // dispatch/catalog actually parses.
    assert!(
        !body.contains("/prompt-template"),
        "prompt-templates summary must not advertise the non-command `/prompt-template`: {body}"
    );
    assert!(
        body.contains("/review"),
        "prompt-templates summary must show the real `/<name>` slash form: {body}"
    );
}

#[test]
fn chunk_doc_sections_keeps_leading_preamble_as_headingless_section() {
    let doc = BundledDoc {
        path: "docs/external/PRE.md",
        content: "preamble text before any heading\n\n## First\n\nbody\n",
    };
    let sections = chunk_doc_sections(&doc);
    assert_eq!(sections.len(), 2);
    assert_eq!(sections[0].heading, "");
    assert!(sections[0].content.contains("preamble text"));
    assert_eq!(sections[1].heading, "## First");
}

#[test]
fn chunk_doc_sections_skips_empty_doc() {
    let doc = BundledDoc {
        path: "docs/external/EMPTY.md",
        content: "   \n\n  \n",
    };
    assert!(chunk_doc_sections(&doc).is_empty());
}

#[test]
fn relevant_doc_sections_scopes_to_matched_topic() {
    // Explicit /help providers → sections must come only from the providers
    // topic's cited docs + anchors, never from an unrelated doc like SESSIONS.md.
    let sections = relevant_doc_sections_for_input("/help providers");
    assert!(!sections.is_empty(), "providers topic must yield sections");

    let paths: BTreeSet<&str> = sections.iter().map(|s| s.path).collect();
    assert!(
        paths.contains("docs/external/PROVIDERS.md"),
        "providers sections must include PROVIDERS.md: {paths:?}"
    );
    assert!(
        !paths.contains("docs/external/SESSIONS.md"),
        "providers sections must NOT include unrelated SESSIONS.md: {paths:?}"
    );

    // Every returned section path must be one of the candidate docs for this
    // topic (sections are scoped, not drawn from the whole corpus).
    let candidate_paths: BTreeSet<&str> = relevant_docs_for_input("/help providers")
        .iter()
        .map(|d| d.path)
        .collect();
    for section in &sections {
        assert!(
            candidate_paths.contains(section.path),
            "section path {} must be a candidate doc",
            section.path
        );
    }
}

#[test]
fn relevant_doc_sections_ranks_on_topic_section_first() {
    // A query token that strongly matches one specific section's content should
    // surface an on-topic section near the top. Use the providers topic and a
    // token ("anthropic") that appears in the PROVIDERS doc.
    let sections = relevant_doc_sections_for_input("/help providers anthropic bedrock");
    assert!(!sections.is_empty());

    // The highest-ranked section should mention one of the query tokens.
    let top = &sections[0];
    let top_lower = top.content.to_ascii_lowercase();
    assert!(
        top_lower.contains("anthropic")
            || top_lower.contains("bedrock")
            || top_lower.contains("provider"),
        "top section should be on-topic: heading={:?}",
        top.heading
    );

    // Section count stays bounded (cap is ~10).
    assert!(
        sections.len() <= 10,
        "section count must respect the cap: got {}",
        sections.len()
    );
}

#[test]
fn relevant_doc_sections_keeps_doc_diversity() {
    // A topic whose cited docs + anchors span multiple files should return
    // sections from more than one doc (diversity pass), not all from one doc.
    let sections = relevant_doc_sections_for_input("/help permissions");
    assert!(!sections.is_empty());
    let distinct_docs: BTreeSet<&str> = sections.iter().map(|s| s.path).collect();
    assert!(
        distinct_docs.len() >= 2,
        "permissions sections should span multiple docs for diversity: {distinct_docs:?}"
    );
}

#[test]
fn help_citation_url_serde_round_trips_as_kind_url() {
    let citation = HelpCitation::Url("https://squeezyagent.com/docs/".to_string());
    let json = serde_json::to_value(&citation).expect("serialize Url citation");
    assert_eq!(json["kind"], "url", "{json}");
    assert_eq!(json["value"], "https://squeezyagent.com/docs/", "{json}");

    let back: HelpCitation = serde_json::from_value(json).expect("deserialize Url citation");
    assert_eq!(back, citation);
}

#[test]
fn help_citation_url_renders_bare_url_not_markdown_link() {
    let citation = HelpCitation::Url("https://example.com/page".to_string());
    let rendered = citation.render();
    // Bare URL only: the TUI auto-linkifies plain https text, so a markdown
    // `[text](url)` wrapper would double-render.
    assert!(rendered.contains("https://example.com/page"), "{rendered}");
    assert!(
        !rendered.contains("]("),
        "must not be a markdown link: {rendered}"
    );
    assert!(
        !rendered.contains('['),
        "must not be a markdown link: {rendered}"
    );
}

#[test]
fn help_answer_source_doc_help_web_label() {
    assert_eq!(HelpAnswerSource::DocHelpWeb.label(), "doc-help web answer");
    // Sanity-check the siblings stay stable alongside the new variant.
    assert_eq!(
        HelpAnswerSource::LocalCurated.label(),
        "local curated answer"
    );
    assert_eq!(
        HelpAnswerSource::DocHelpModel.label(),
        "doc-help model answer"
    );
}

#[test]
fn from_rendered_label_recovers_the_source_from_a_rendered_answer() {
    // Round-trip: a rendered answer's trailing `*<label>*` line decodes back to
    // its source, so the TUI can re-derive the source of a model-backed answer
    // that reached it as a plain assistant message.
    for source in [
        HelpAnswerSource::LocalCurated,
        HelpAnswerSource::DocHelpModel,
        HelpAnswerSource::DocHelpWeb,
    ] {
        let answer = HelpAnswer {
            topic: "round-trip".to_string(),
            status: HelpStatus::Answered,
            body: "Body text.".to_string(),
            citations: Vec::new(),
            config_sections: Vec::new(),
            source,
        };
        let rendered = answer.render_markdown();
        assert_eq!(
            HelpAnswerSource::from_rendered_label(&rendered),
            Some(source),
            "round-trip failed for {source:?}: {rendered}"
        );
    }
    // No known label → no source (an ordinary assistant message is not a help
    // answer).
    assert_eq!(
        HelpAnswerSource::from_rendered_label("Just a normal answer with no label."),
        None
    );
}

#[test]
fn help_answer_renders_url_citation_in_sources_list() {
    let answer = HelpAnswer {
        topic: "web".to_string(),
        status: HelpStatus::Answered,
        body: "Web-grounded answer body.".to_string(),
        citations: vec![HelpCitation::Url("https://example.com/article".to_string())],
        config_sections: Vec::new(),
        source: HelpAnswerSource::DocHelpWeb,
    };
    let rendered = answer.render_markdown();
    assert!(rendered.contains("**Sources:**"), "{rendered}");
    assert!(
        rendered.contains("https://example.com/article"),
        "{rendered}"
    );
    assert!(rendered.contains("doc-help web answer"), "{rendered}");
}

#[test]
fn repo_url_and_slug_stay_consistent() {
    // SQUEEZY_REPO_SLUG is the single source of truth for the published repo;
    // the URL must end with it so callers can build raw/api/host strings from
    // either without them drifting apart on a rename.
    assert!(
        super::SQUEEZY_REPO_URL.ends_with(super::SQUEEZY_REPO_SLUG),
        "SQUEEZY_REPO_URL ({}) must end with SQUEEZY_REPO_SLUG ({})",
        super::SQUEEZY_REPO_URL,
        super::SQUEEZY_REPO_SLUG,
    );
}
