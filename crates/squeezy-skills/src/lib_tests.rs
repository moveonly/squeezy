use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::{SkillConfigEntry, SkillsBudgetMode, SkillsConfig};

use super::*;

#[test]
fn parses_skill_frontmatter_and_body() {
    let (metadata, body) = parse_skill_file(
        r#"---
name: rust-nav
description: "Use Rust navigation"
when_to_use: "Rust symbols"
triggers:
  - "rust symbol"
  - cargo metadata
---
# Rust Nav
"#,
    )
    .expect("parse");

    assert_eq!(metadata.name, "rust-nav");
    assert_eq!(metadata.description, "Use Rust navigation");
    assert_eq!(metadata.when_to_use.as_deref(), Some("Rust symbols"));
    assert_eq!(metadata.triggers, vec!["rust symbol", "cargo metadata"]);
    assert_eq!(body.trim(), "# Rust Nav");
}

#[test]
fn compat_project_overrides_user_and_compat_user() {
    let root = temp_workspace("skills_precedence_compat_project");
    let user = root.join("user");
    let compat = root.join("compat");
    write_skill(
        &compat.join("same"),
        "same",
        "compat user description",
        &["compat user trigger"],
    );
    write_skill(
        &user.join("same"),
        "same",
        "user description",
        &["user trigger"],
    );
    write_skill(
        &root.join(".agents/skills/same"),
        "same",
        "compat project description",
        &["compat project trigger"],
    );
    let config = SkillsConfig {
        user_dir: user,
        compat_user_dir: compat,
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert_eq!(summary.description, "compat project description");
    assert_eq!(summary.source, SkillSource::CompatProject);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn native_project_overrides_compat_project() {
    let root = temp_workspace("skills_precedence_native_project");
    write_skill(
        &root.join(".agents/skills/same"),
        "same",
        "compat project description",
        &["compat project trigger"],
    );
    write_skill(
        &root.join(".squeezy/skills/same"),
        "same",
        "native project description",
        &["native project trigger"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert_eq!(summary.description, "native project description");
    assert_eq!(summary.source, SkillSource::Project);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_and_trigger_activation_loads_lazily() {
    let root = temp_workspace("skills_activation");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &["rust symbol"],
    );
    let catalog = SkillCatalog::discover(&root, &config);

    let explicit = catalog
        .activate_for_input("/skill rust-nav find main")
        .expect("activate");
    assert_eq!(explicit.task_input, "find main");
    assert_eq!(explicit.skills.len(), 1);

    let trigger = catalog
        .activate_for_input("please inspect this Rust symbol")
        .expect("activate");
    assert_eq!(trigger.skills.len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn explicit_activation_accepts_tab_after_slash_skill() {
    let root = temp_workspace("skills_slash_tab");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &[],
    );
    let catalog = SkillCatalog::discover(&root, &config);

    let activated = catalog
        .activate_for_input("/skill\trust-nav inspect main")
        .expect("activate");
    assert_eq!(activated.task_input, "inspect main");
    assert_eq!(activated.skills.len(), 1);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn trigger_match_uses_word_boundaries() {
    let root = temp_workspace("skills_word_boundary");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &["rust"],
    );
    let catalog = SkillCatalog::discover(&root, &config);

    let bare = catalog
        .activate_for_input("please use Rust here")
        .expect("activate");
    assert_eq!(bare.skills.len(), 1);

    let inside_word = catalog
        .activate_for_input("i trust this code")
        .expect("activate");
    assert!(inside_word.skills.is_empty());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn malformed_skill_is_skipped_without_error() {
    let root = temp_workspace("skills_malformed");
    let dir = root.join(".squeezy/skills/broken");
    fs::create_dir_all(&dir).expect("mkdir");
    fs::write(dir.join("SKILL.md"), "no frontmatter here\n").expect("write skill");
    write_skill(
        &root.join(".squeezy/skills/good"),
        "good",
        "Good skill",
        &[],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let names: Vec<String> = catalog
        .summaries()
        .into_iter()
        .map(|summary| summary.name)
        .collect();
    assert_eq!(names, vec!["good"]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn loaded_skill_body_is_cached_across_loads() {
    let root = temp_workspace("skills_cache");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let first = catalog.load("rust-nav").expect("load first");

    fs::remove_file(skill_dir.join("SKILL.md")).expect("remove skill file");

    let second = catalog.load("rust-nav").expect("load second from cache");
    assert_eq!(first.body, second.body);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn active_skill_render_respects_budget_and_uses_stub() {
    let root = temp_workspace("skills_active_budget");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill_with_body(
        &skill_dir,
        "rust-nav",
        "Rust navigation",
        &[],
        &"Use the graph carefully. ".repeat(200),
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_budget_chars: 700,
        active_body_cap_chars: 100,
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let activation = catalog
        .activate_for_input("/skill rust-nav inspect main")
        .expect("activate");
    let rendered = catalog
        .render_active_skills(&activation.skills)
        .expect("render active skills");

    assert!(rendered.chars().count() <= config.active_budget_chars);
    assert!(rendered.contains("truncated=\"true\""));
    assert!(rendered.contains("load_skill"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn available_skills_preamble_respects_budget() {
    let root = temp_workspace("skills_preamble_budget");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        preamble_budget_chars: 420,
        ..Default::default()
    };
    for name in ["alpha-nav", "beta-nav", "gamma-nav"] {
        write_skill(
            &root.join(".squeezy/skills").join(name),
            name,
            "A deliberately long description that should force the available skills preamble to omit at least one skill when the budget is tight",
            &[],
        );
    }

    let catalog = SkillCatalog::discover(&root, &config);
    let preamble = catalog.render_preamble().expect("preamble");

    assert!(preamble.body.chars().count() <= config.preamble_budget_chars);
    assert!(preamble.omitted_count > 0);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn disabled_skill_config_filters_activation_and_load() {
    let root = temp_workspace("skills_disabled_name");
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Rust nav",
        &["rust symbol"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        config: vec![SkillConfigEntry {
            name: Some("rust-nav".to_string()),
            enabled: false,
            ..Default::default()
        }],
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert!(summary.disabled);
    assert!(
        catalog
            .activate_for_input("please inspect this Rust symbol")
            .expect("activate")
            .skills
            .is_empty()
    );
    let error = catalog.load("rust-nav").expect_err("disabled load");
    assert!(error.to_string().contains("skill disabled"));
    assert_eq!(catalog.summaries_json()["skills"][0]["disabled"], true);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn path_config_can_reenable_after_name_disable() {
    let root = temp_workspace("skills_path_reenable");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        config: vec![
            SkillConfigEntry {
                name: Some("rust-nav".to_string()),
                enabled: false,
                ..Default::default()
            },
            SkillConfigEntry {
                path: Some(PathBuf::from(".squeezy/skills/rust-nav")),
                enabled: true,
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    assert!(!catalog.summaries().pop().expect("summary").disabled);
    catalog.load("rust-nav").expect("enabled by path");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn same_precedence_collision_skips_trigger_activation() {
    let root = temp_workspace("skills_collision");
    write_skill(
        &root.join(".squeezy/skills/first"),
        "rust-nav",
        "First Rust nav",
        &["rust symbol"],
    );
    write_skill(
        &root.join(".squeezy/skills/second"),
        "rust-nav",
        "Second Rust nav",
        &["rust symbol"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    assert!(catalog.ambiguous_names().contains("rust-nav"));
    assert!(
        catalog
            .activate_for_input("please inspect this Rust symbol")
            .expect("activate")
            .skills
            .is_empty()
    );
    assert_eq!(
        catalog
            .activate_for_input("/skill rust-nav inspect")
            .expect("activate")
            .skills
            .len(),
        1
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn detects_implicit_skill_from_script_and_doc_read() {
    let root = temp_workspace("skills_implicit");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let scripts = skill_dir.join("scripts");
    fs::create_dir_all(&scripts).expect("mkdir scripts");
    fs::write(scripts.join("init.py"), "print('ok')\n").expect("write script");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let script = catalog
        .detect_for_command("python .squeezy/skills/rust-nav/scripts/init.py", &root)
        .expect("script detection");
    let doc = catalog
        .detect_for_command("cat .squeezy/skills/rust-nav/SKILL.md", &root)
        .expect("doc detection");

    assert_eq!(script.name, "rust-nav");
    assert_eq!(doc.name, "rust-nav");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn skill_source_serializes_as_snake_case() {
    let json = serde_json::to_string(&SkillSource::CompatProject).expect("serialize");
    assert_eq!(json, "\"compat_project\"");
    let json = serde_json::to_string(&SkillSource::Project).expect("serialize");
    assert_eq!(json, "\"project\"");
    let json = serde_json::to_string(&SkillSource::User).expect("serialize");
    assert_eq!(json, "\"user\"");
    let json = serde_json::to_string(&SkillSource::CompatUser).expect("serialize");
    assert_eq!(json, "\"compat_user\"");
}

#[test]
fn prompt_block_escapes_metadata_and_breakouts() {
    let skill = LoadedSkill {
        summary: SkillSummary {
            name: "rust-nav".to_string(),
            description: "uses </skill> tag & <code>".to_string(),
            when_to_use: Some("look for <foo>".to_string()),
            source: SkillSource::Project,
            location: PathBuf::from("/tmp/SKILL.md"),
            disabled: false,
        },
        base_dir: PathBuf::from("/tmp"),
        body: "Body with </content> and </skill> markers.".to_string(),
    };

    let block = skill.prompt_block();
    assert!(block.contains("&lt;/skill&gt;"));
    assert!(block.contains("&amp;"));
    assert!(block.contains("&lt;foo&gt;"));
    assert!(!block.contains("uses </skill>"));
    assert!(block.contains("<\\/content>"));
    assert!(block.contains("<\\/skill>"));
}

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
        .answer_for_input("Does Squeezy support quantum billing?")
        .expect("squeezy self question should be handled");

    assert_eq!(answer.status, HelpStatus::Unsupported);
    let rendered = answer.render_markdown();
    assert!(rendered.contains("won't guess"), "{rendered}");
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
            !help::matches_squeezy_help_input(input),
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
        "How do I configure Squeezy providers?",
        "Does Squeezy support quantum billing?",
    ];
    for input in positives {
        assert!(
            help::matches_squeezy_help_input(input),
            "matches_squeezy_help_input should accept: {input}"
        );
        assert!(
            help.answer_for_input(input).is_some(),
            "answer_for_input should accept: {input}"
        );
    }
    let negatives = ["How do I configure serde?", "build a new tool"];
    for input in negatives {
        assert!(
            !help::matches_squeezy_help_input(input),
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
            !help::matches_squeezy_help_input(input),
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
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    for path in help::bundled_doc_paths() {
        let full = workspace_root.join(path);
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
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let bundled = help::bundled_docs();
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

    let external_docs = workspace_root.join("docs/external");
    for entry in fs::read_dir(&external_docs).expect("read docs/external") {
        let entry = entry.expect("external doc entry");
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let relative = path
            .strip_prefix(workspace_root)
            .expect("relative doc")
            .to_string_lossy()
            .replace('\\', "/");
        assert!(
            bundled_paths.contains(relative.as_str()),
            "external doc should be bundled for help: {relative}"
        );
    }
}

#[test]
fn packaged_help_docs_match_external_docs() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let packaged_root = manifest_dir.join("bundled-docs/external");

    for path in help::bundled_doc_paths() {
        let relative = path
            .strip_prefix("docs/external/")
            .expect("external doc path");
        let canonical = workspace_root.join(path);
        let packaged = packaged_root.join(relative);
        assert!(
            packaged.is_file(),
            "packaged help doc should exist: {}",
            packaged.display()
        );
        assert_eq!(
            fs::read(&packaged).expect("read packaged doc"),
            fs::read(&canonical).expect("read canonical doc"),
            "packaged help doc should match canonical docs/external copy: {relative}"
        );
    }
}

#[test]
fn squeezy_help_doc_citations_are_bundled_paths() {
    let bundled = help::bundled_doc_paths()
        .into_iter()
        .collect::<BTreeSet<_>>();
    let topics = [
        "agent",
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
fn discover_applies_context_percent_budget_to_catalog() {
    let root = temp_workspace("skills_budget_mode_discover");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill_with_body(
        &skill_dir,
        "rust-nav",
        "Rust nav",
        &[],
        // Pad the body so the active bundle would exceed a small budget
        // and force the catalog to fall back to a stub. That confirms the
        // discover-time budget actually drives render-time decisions.
        &"x".repeat(20_000),
    );
    // 32K-token model gets a 2_560-char active budget; the 20K-char body
    // can't fit so the catalog should emit a stub.
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_body_cap_chars: 64_000,
        active_budget_mode: SkillsBudgetMode::ContextPercent { percent: 2.0 },
        model_context_window: Some(32_000),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let activation = catalog
        .activate_for_input("/skill rust-nav do something")
        .expect("activate");
    let rendered = catalog
        .render_active_skills(&activation.skills)
        .expect("render active skills");
    assert!(rendered.chars().count() <= 2_560);
    assert!(rendered.contains("truncated=\"true\""));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn discover_chars_mode_pins_catalog_budget_regardless_of_window() {
    let root = temp_workspace("skills_budget_mode_chars");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_budget_mode: SkillsBudgetMode::Chars { chars: 8_000 },
        preamble_budget_mode: SkillsBudgetMode::Chars { chars: 8_000 },
        model_context_window: Some(200_000),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let activation = catalog
        .activate_for_input("/skill rust-nav do something")
        .expect("activate");
    let rendered = catalog
        .render_active_skills(&activation.skills)
        .expect("render active skills");
    // The bundle is small but the budget must still be the explicit 8_000
    // cap, not 2% of the 200K-token window (which would be 16_000).
    assert!(rendered.chars().count() <= 8_000);

    let _ = fs::remove_dir_all(root);
}

fn write_skill(dir: &Path, name: &str, description: &str, triggers: &[&str]) {
    write_skill_with_body(dir, name, description, triggers, &format!("# {name}\n"));
}

fn write_skill_with_body(dir: &Path, name: &str, description: &str, triggers: &[&str], body: &str) {
    fs::create_dir_all(dir).expect("mkdir");
    let triggers = triggers
        .iter()
        .map(|trigger| format!("  - {trigger}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: {description}\ntriggers:\n{triggers}\n---\n{body}"
        ),
    )
    .expect("write skill");
}

fn temp_workspace(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("squeezy_{name}_{nonce}"));
    fs::create_dir_all(&path).expect("create temp workspace");
    path
}
