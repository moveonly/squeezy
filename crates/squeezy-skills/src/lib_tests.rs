use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

// `json!` is only referenced by `#[cfg(unix)]` tests below; gate the
// import the same way so Windows builds (which exclude those tests)
// don't fail under `-D warnings`.
#[cfg(unix)]
use serde_json::json;
use squeezy_core::{SkillConfigEntry, SkillsBudgetMode, SkillsConfig};
use squeezy_hooks::{HookEvent, HookRegistry};
use tracing_subscriber::fmt::MakeWriter;

use super::*;

#[test]
fn bundled_skills_load_with_valid_metadata() {
    let bundled = bundled_skills();
    assert!(
        bundled.len() >= 2,
        "expected at least two bundled samples, got {}",
        bundled.len()
    );
    let mut seen = BTreeSet::new();
    for skill in &bundled {
        assert!(
            is_valid_skill_name(&skill.summary.name),
            "bundled skill name {} must satisfy SKILL.md naming rules",
            skill.summary.name
        );
        assert!(
            !skill.summary.description.trim().is_empty(),
            "bundled skill {} must have a non-empty description",
            skill.summary.name
        );
        assert!(
            !skill.body.trim().is_empty(),
            "bundled skill {} must have a non-empty body",
            skill.summary.name
        );
        assert!(
            seen.insert(skill.summary.name.clone()),
            "duplicate bundled skill name: {}",
            skill.summary.name
        );
        assert!(
            skill.base_dir.starts_with("<squeezy-builtin>"),
            "bundled skill {} base_dir must be the in-binary sentinel root, got {}",
            skill.summary.name,
            skill.base_dir.display()
        );
    }
}

#[test]
fn customize_skill_loads_when_user_edits_config_toml() {
    // Acceptance test for the bundled `customize-squeezy` skill: it must
    // be present in the in-binary sample list, and when installed into a
    // discovered skills root it must activate on the kind of input a user
    // gives when they ask to edit `settings.toml` / `squeezy.toml`.
    let bundled = bundled_skills();
    let customize = bundled
        .iter()
        .find(|skill| skill.summary.name == "customize-squeezy")
        .expect("customize-squeezy bundled skill is registered");
    assert!(
        customize
            .body
            .to_ascii_lowercase()
            .contains("settings.toml"),
        "bundled customize-squeezy body should document settings.toml edits"
    );

    let root = temp_workspace("skills_customize_squeezy_loads");
    let user_dir = root.join("user");
    let skill_dir = user_dir.join("customize-squeezy");
    fs::create_dir_all(&skill_dir).expect("mkdir");
    fs::write(
        skill_dir.join("SKILL.md"),
        include_str!("../builtin/customize-squeezy/SKILL.md"),
    )
    .expect("install bundled skill");

    let config = SkillsConfig {
        user_dir,
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };
    let catalog = SkillCatalog::discover(&root, &config);
    let activation = catalog
        .activate_for_input("please edit my ~/.squeezy/settings.toml to add a provider")
        .expect("activate");
    assert!(
        activation
            .skills
            .iter()
            .any(|loaded| loaded.summary.name == "customize-squeezy"),
        "expected customize-squeezy to activate on a settings.toml edit prompt, got {:?}",
        activation
            .skills
            .iter()
            .map(|loaded| loaded.summary.name.as_str())
            .collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn bundled_skills_render_through_catalog_helpers() {
    let bundled = bundled_skills();
    let block = bundled
        .first()
        .expect("at least one bundled skill")
        .prompt_block();
    assert!(block.contains("<skill name=\""));
    assert!(block.contains("<content>"));
}

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
    assert_eq!(explicit.kinds, vec![SkillActivationKind::Explicit]);

    let trigger = catalog
        .activate_for_input("please inspect this Rust symbol")
        .expect("activate");
    assert_eq!(trigger.skills.len(), 1);
    assert_eq!(trigger.kinds, vec![SkillActivationKind::Trigger]);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn activation_kind_serializes_as_snake_case() {
    let cases = [
        (SkillActivationKind::Explicit, "\"explicit\""),
        (SkillActivationKind::Trigger, "\"trigger\""),
        (SkillActivationKind::ImplicitShell, "\"implicit_shell\""),
    ];
    for (kind, expected) in cases {
        let json = serde_json::to_string(&kind).expect("serialize");
        assert_eq!(json, expected);
    }
}

#[test]
fn explicit_then_trigger_dedup_keeps_explicit_kind() {
    let root = temp_workspace("skills_activation_kind_dedup");
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

    // Input names the skill explicitly *and* the trigger phrase matches.
    // Dedup must keep the first occurrence (Explicit) so telemetry reports
    // the strongest signal, not the incidental trigger hit.
    let activation = catalog
        .activate_for_input("/skill rust-nav inspect this rust symbol")
        .expect("activate");
    assert_eq!(activation.skills.len(), 1);
    assert_eq!(activation.kinds, vec![SkillActivationKind::Explicit]);

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
fn active_skill_render_redistributes_descriptions_to_preserve_roster() {
    // Three skills, each with body well above the body cap so every skill
    // starts as a stub. Budget is sized so that the full-description stubs
    // overflow the active block but the minimum-line stubs all fit — the
    // redistribute step must keep every skill present and give each a
    // non-empty description.
    let root = temp_workspace("skills_active_redistribute");
    let names = ["alpha-skill", "beta-skill", "gamma-skill"];
    let descriptions = [
        "Alpha description that is long enough to occupy several stub characters when budget allows.",
        "Beta description, also reasonably long so we can verify the redistribute loop allocates across skills.",
        "Gamma description with comparable length, exercising the third allocation slot of the loop.",
    ];
    for (name, description) in names.iter().zip(descriptions.iter()) {
        write_skill_with_body(
            &root.join(".squeezy/skills").join(name),
            name,
            description,
            &[],
            &"body line that exceeds the cap. ".repeat(60),
        );
    }
    // Compute the minimum-stub floor at runtime so the test budget is robust
    // against temp-path length variation across hosts. The full-description
    // aggregate is the floor plus the description chars themselves — sit
    // between the two so the redistribute step is required.
    let catalog = SkillCatalog::discover(
        &root,
        &SkillsConfig {
            user_dir: root.join("user"),
            compat_user_dir: root.join("compat"),
            active_budget_chars: usize::MAX,
            active_body_cap_chars: 100,
            ..Default::default()
        },
    );
    let loaded = names
        .iter()
        .map(|name| catalog.load(name).expect("load"))
        .collect::<Vec<_>>();
    let full_block = render::render_active_skills(&loaded, usize::MAX, 100)
        .expect("baseline render with unbounded budget");
    // Each description is ASCII without XML-special chars so xml_escape is
    // identity; subtracting the description-char sum from the full render
    // yields the minimum-stub floor (description text removed, structure
    // kept).
    let desc_payload_chars: usize = descriptions
        .iter()
        .map(|description| description.chars().count())
        .sum();
    let floor = full_block.chars().count() - desc_payload_chars;
    // Pick a budget strictly between the floor and the full aggregate so the
    // redistribute branch — not the all-fits-as-is branch and not the
    // drop-skills branch — is the one exercised.
    let active_budget_chars = floor + desc_payload_chars / 2;

    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        active_budget_chars,
        active_body_cap_chars: 100,
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let loaded = names
        .iter()
        .map(|name| catalog.load(name).expect("load"))
        .collect::<Vec<_>>();
    let rendered = catalog
        .render_active_skills(&loaded)
        .expect("render active skills");

    assert!(rendered.chars().count() <= config.active_budget_chars);
    for name in &names {
        let needle = format!("name=\"{name}\"");
        assert!(
            rendered.contains(&needle),
            "redistribute must keep every skill present; missing {name}\n---\n{rendered}\n---"
        );
    }
    // No skill should have a completely empty description tag — the loop
    // must allocate at least one char to each skill when the budget allows
    // it. Empty `<description></description>` indicates the skill was kept
    // at the minimum-stub floor.
    assert!(
        !rendered.contains("<description></description>"),
        "redistribute must allocate at least one description char per skill\n---\n{rendered}\n---"
    );

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
            manifest: None,
            context_mode: SkillContextMode::Inline,
        },
        base_dir: PathBuf::from("/tmp"),
        body: "Body with </content> and </skill> markers.".to_string(),
        hooks: BTreeMap::new(),
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
fn parses_context_fork_frontmatter() {
    let (metadata, _body) = parse_skill_file(
        r#"---
name: review-spec
description: "Run a multi-step review"
context: fork
---
# body
"#,
    )
    .expect("parse");
    assert_eq!(metadata.context_mode, SkillContextMode::Fork);
}

#[test]
fn missing_context_defaults_to_inline() {
    let (metadata, _body) = parse_skill_file(
        r#"---
name: inline-skill
description: "Plain skill"
---
# body
"#,
    )
    .expect("parse");
    assert_eq!(metadata.context_mode, SkillContextMode::Inline);
}

#[test]
fn unrecognised_context_value_falls_back_to_inline() {
    let (metadata, _body) = parse_skill_file(
        r#"---
name: typo-skill
description: "Author typo in context"
context: bogus
---
# body
"#,
    )
    .expect("parse");
    assert_eq!(metadata.context_mode, SkillContextMode::Inline);
}

#[test]
fn explicit_inline_context_parses() {
    let (metadata, _body) = parse_skill_file(
        r#"---
name: explicit-inline
description: "Author wrote inline explicitly"
context: "inline"
---
# body
"#,
    )
    .expect("parse");
    assert_eq!(metadata.context_mode, SkillContextMode::Inline);
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
    let docs_dir = manifest_dir.join("external-docs");
    for path in help::bundled_doc_paths() {
        let file_name = path
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

#[test]
fn parses_skill_manifest_with_all_fields() {
    let manifest = parse_skill_manifest(
        r#"
tool_deps = ["mcp:exa", "shell"]
icon = "assets/icon.png"
prompt_hint = "Use this when navigating the repo."
"#,
    )
    .expect("parse manifest");

    assert_eq!(manifest.tool_deps, vec!["mcp:exa", "shell"]);
    assert_eq!(manifest.icon, Some(PathBuf::from("assets/icon.png")));
    assert_eq!(
        manifest.prompt_hint.as_deref(),
        Some("Use this when navigating the repo.")
    );
}

#[test]
fn empty_skill_manifest_parses_into_default() {
    let manifest = parse_skill_manifest("").expect("empty toml parses");
    assert!(manifest.tool_deps.is_empty());
    assert!(manifest.icon.is_none());
    assert!(manifest.prompt_hint.is_none());
}

#[test]
fn malformed_skill_manifest_returns_error() {
    let error = parse_skill_manifest("tool_deps = not a list\n").expect_err("invalid toml");
    assert!(!error.is_empty());
}

#[test]
fn manifest_is_attached_to_summary_during_discovery() {
    let root = temp_workspace("skills_manifest_attached");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    fs::write(
        skill_dir.join("skill.toml"),
        r#"tool_deps = ["mcp:exa"]
icon = "assets/icon.png"
prompt_hint = "Use the graph carefully."
"#,
    )
    .expect("write skill.toml");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    let manifest = summary.manifest.expect("manifest attached");
    assert_eq!(manifest.tool_deps, vec!["mcp:exa"]);
    assert_eq!(manifest.icon, Some(PathBuf::from("assets/icon.png")));
    assert_eq!(
        manifest.prompt_hint.as_deref(),
        Some("Use the graph carefully.")
    );

    let json = catalog.summaries_json();
    let entry = &json["skills"][0];
    assert_eq!(entry["manifest"]["tool_deps"][0], "mcp:exa");
    assert_eq!(entry["manifest"]["icon"], "assets/icon.png");
    assert_eq!(entry["manifest"]["prompt_hint"], "Use the graph carefully.");

    let _ = fs::remove_dir_all(root);
}

#[test]
fn manifest_tool_deps_and_hint_surface_in_prompt_block() {
    let root = temp_workspace("skills_manifest_prompt");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    fs::write(
        skill_dir.join("skill.toml"),
        r#"tool_deps = ["mcp:exa", "web_fetch"]
prompt_hint = "Activate for Rust graph questions."
"#,
    )
    .expect("write skill.toml");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let loaded = catalog.load("rust-nav").expect("load");
    let block = loaded.prompt_block();

    assert!(block.contains("<manifest>"), "{block}");
    assert!(block.contains("<tool>mcp:exa</tool>"), "{block}");
    assert!(block.contains("<tool>web_fetch</tool>"), "{block}");
    assert!(
        block.contains("<prompt_hint>Activate for Rust graph questions.</prompt_hint>"),
        "{block}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn missing_skill_manifest_leaves_summary_unaffected() {
    let root = temp_workspace("skills_manifest_missing");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert!(summary.manifest.is_none());

    let loaded = catalog.load("rust-nav").expect("load");
    assert!(!loaded.prompt_block().contains("<manifest>"));

    let _ = fs::remove_dir_all(root);
}

#[test]
fn malformed_skill_manifest_is_skipped_without_dropping_skill() {
    let root = temp_workspace("skills_manifest_malformed");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    fs::write(skill_dir.join("skill.toml"), "tool_deps = not a list\n").expect("write skill.toml");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert_eq!(summary.name, "rust-nav");
    assert!(summary.manifest.is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn empty_skill_manifest_does_not_attach_to_summary() {
    let root = temp_workspace("skills_manifest_empty_file");
    let skill_dir = root.join(".squeezy/skills/rust-nav");
    write_skill(&skill_dir, "rust-nav", "Rust nav", &[]);
    fs::write(skill_dir.join("skill.toml"), "").expect("write empty skill.toml");
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let summary = catalog.summaries().pop().expect("summary");
    assert!(summary.manifest.is_none());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn skill_manifest_unknown_keys_are_rejected() {
    // Strict mode keeps the format auditable: typos in field names must
    // surface so a misspelled `tools_dep` doesn't silently drop the
    // declared dependency.
    let error = parse_skill_manifest("bogus_field = 1\n").expect_err("unknown key");
    assert!(
        error.contains("unknown") || error.contains("bogus"),
        "{error}"
    );
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

#[derive(Clone, Default)]
struct SharedLogWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl SharedLogWriter {
    fn contents(&self) -> String {
        let bytes = self.buffer.lock().expect("log buffer").clone();
        String::from_utf8(bytes).expect("logs are UTF-8")
    }
}

impl<'writer> MakeWriter<'writer> for SharedLogWriter {
    type Writer = SharedLogWrite;

    fn make_writer(&'writer self) -> Self::Writer {
        SharedLogWrite {
            buffer: self.buffer.clone(),
        }
    }
}

struct SharedLogWrite {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl io::Write for SharedLogWrite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer
            .lock()
            .expect("log buffer")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn capture_discover_logs(workspace: &Path, config: &SkillsConfig) -> (SkillCatalog, String) {
    let writer = SharedLogWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(writer.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();
    let catalog =
        tracing::subscriber::with_default(subscriber, || SkillCatalog::discover(workspace, config));
    (catalog, writer.contents())
}

#[test]
fn same_precedence_name_collision_emits_load_time_warning() {
    let root = temp_workspace("skills_warn_same_precedence_name");
    write_skill(
        &root.join(".squeezy/skills/first"),
        "rust-nav",
        "First Rust nav",
        &[],
    );
    write_skill(
        &root.join(".squeezy/skills/second"),
        "rust-nav",
        "Second Rust nav",
        &[],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let (catalog, logs) = capture_discover_logs(&root, &config);
    assert!(catalog.ambiguous_names().contains("rust-nav"));
    assert!(
        logs.contains("same-precedence skill name collision"),
        "missing same-precedence warning: {logs}"
    );
    assert!(
        logs.contains("name=rust-nav"),
        "missing skill name field: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn cross_precedence_name_collision_emits_load_time_warning() {
    let root = temp_workspace("skills_warn_cross_precedence_name");
    write_skill(
        &root.join(".agents/skills/rust-nav"),
        "rust-nav",
        "Compat project Rust nav",
        &[],
    );
    write_skill(
        &root.join(".squeezy/skills/rust-nav"),
        "rust-nav",
        "Native project Rust nav",
        &[],
    );
    let config = SkillsConfig {
        user_dir: root.join("user-noop"),
        compat_user_dir: root.join("compat-noop"),
        ..Default::default()
    };

    let (catalog, logs) = capture_discover_logs(&root, &config);
    // The native-project copy wins; the compat copy is shadowed.
    let summary = catalog.summaries().pop().expect("summary");
    assert_eq!(summary.source, SkillSource::Project);
    assert!(
        logs.contains("skill name reused at higher precedence"),
        "missing cross-precedence warning: {logs}"
    );
    assert!(
        logs.contains("overriding_source=\"project\"")
            || logs.contains("overriding_source=project"),
        "missing overriding source field: {logs}"
    );
    assert!(
        logs.contains("overridden_source=\"compat_project\"")
            || logs.contains("overridden_source=compat_project"),
        "missing overridden source field: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn duplicate_trigger_across_skills_emits_load_time_warning() {
    let root = temp_workspace("skills_warn_trigger_collision");
    write_skill(
        &root.join(".squeezy/skills/alpha"),
        "alpha",
        "Alpha skill",
        &["graph"],
    );
    write_skill(
        &root.join(".squeezy/skills/beta"),
        "beta",
        "Beta skill",
        &["GRAPH"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let (_catalog, logs) = capture_discover_logs(&root, &config);
    assert!(
        logs.contains("duplicate skill trigger"),
        "missing trigger collision warning: {logs}"
    );
    assert!(
        logs.contains("trigger=graph"),
        "missing trigger field (case-folded): {logs}"
    );
    assert!(
        logs.contains("\"alpha\"") && logs.contains("\"beta\""),
        "missing colliding skill names: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn trigger_warning_skips_single_skill_with_repeated_trigger() {
    let root = temp_workspace("skills_warn_no_self_trigger");
    write_skill(
        &root.join(".squeezy/skills/alpha"),
        "alpha",
        "Alpha skill",
        &["graph", "GRAPH"],
    );
    let config = SkillsConfig {
        user_dir: root.join("user"),
        compat_user_dir: root.join("compat"),
        ..Default::default()
    };

    let (_catalog, logs) = capture_discover_logs(&root, &config);
    assert!(
        !logs.contains("duplicate skill trigger"),
        "trigger collision must require two distinct skills: {logs}"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn parses_hooks_block_with_matchers_and_specs() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: validator\ndescription: \"validates bash\"\nhooks:\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"scripts/validate.sh\"\n          once: false\n        - type: command\n          command: \"scripts/log.sh\"\n          once: true\n  PostToolUse:\n    - matcher: \"*\"\n      hooks:\n        - type: command\n          command: \"scripts/audit.sh\"\n---\n# body\n",
    )
    .expect("parse");

    let pre = metadata
        .hooks
        .get(&HookEvent::PreToolUse)
        .expect("PreToolUse parsed");
    assert_eq!(pre.len(), 1);
    assert_eq!(pre[0].matcher.as_deref(), Some("Bash"));
    assert_eq!(pre[0].hooks.len(), 2);
    assert_eq!(pre[0].hooks[0].command, "scripts/validate.sh");
    assert!(!pre[0].hooks[0].once);
    assert_eq!(pre[0].hooks[1].command, "scripts/log.sh");
    assert!(pre[0].hooks[1].once);

    let post = metadata
        .hooks
        .get(&HookEvent::PostToolUse)
        .expect("PostToolUse parsed");
    assert_eq!(post.len(), 1);
    // The literal `*` is normalised to `None` so the handler fires for
    // every payload of the event without per-call filter overhead.
    assert!(post[0].matcher.is_none());
    assert_eq!(post[0].hooks[0].command, "scripts/audit.sh");
}

#[test]
fn parses_hooks_block_drops_unknown_event_without_failing_load() {
    let (metadata, _body) = parse_skill_file(
        "---\nname: validator\ndescription: \"d\"\nhooks:\n  NoSuchEvent:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"scripts/x.sh\"\n  PreToolUse:\n    - matcher: \"Bash\"\n      hooks:\n        - type: command\n          command: \"scripts/y.sh\"\n---\n# body\n",
    )
    .expect("parse");
    assert!(metadata.hooks.contains_key(&HookEvent::PreToolUse));
    assert_eq!(metadata.hooks.len(), 1);
}

#[test]
fn register_skill_hooks_installs_one_handler_per_spec() {
    let skill = LoadedSkill {
        summary: SkillSummary {
            name: "validator".to_string(),
            description: "d".to_string(),
            when_to_use: None,
            source: SkillSource::Project,
            location: PathBuf::from("/tmp/SKILL.md"),
            disabled: false,
            manifest: None,
            context_mode: SkillContextMode::Inline,
        },
        base_dir: PathBuf::from("/tmp"),
        body: String::new(),
        hooks: BTreeMap::from([(
            HookEvent::PreToolUse,
            vec![SkillHookMatcher {
                matcher: Some("Bash".to_string()),
                hooks: vec![
                    SkillHookSpec {
                        command: "true".to_string(),
                        once: false,
                    },
                    SkillHookSpec {
                        command: "true".to_string(),
                        once: true,
                    },
                ],
            }],
        )]),
    };
    let mut registry = HookRegistry::new();
    let installed = register_skill_hooks(&skill, &mut registry);
    assert_eq!(installed, 2);
    assert_eq!(registry.len(), 2);
}

#[cfg(unix)]
#[test]
fn skill_hook_fires_on_matching_event_and_skips_others() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_workspace("skill_hook_fires");
    let marker = root.join("ran");
    let script = root.join("hook.sh");
    fs::write(
        &script,
        format!("#!/bin/sh\necho fired > {}\n", marker.display()),
    )
    .expect("write hook script");
    let mut perms = fs::metadata(&script).expect("script meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod hook");

    let spec = SkillHookSpec {
        command: script.display().to_string(),
        once: false,
    };
    let handler = SkillHookHandler::new(
        "validator".to_string(),
        HookEvent::PreToolUse,
        Some("Bash".to_string()),
        spec,
        root.clone(),
    );
    let mut registry = HookRegistry::new();
    registry.register(Box::new(handler));

    // Non-matching event does not run the script.
    let _ = registry.dispatch(HookEvent::PostToolUse, json!({ "tool_name": "Bash" }));
    assert!(!marker.exists());
    // Matching event with the wrong tool also skips.
    let _ = registry.dispatch(HookEvent::PreToolUse, json!({ "tool_name": "Edit" }));
    assert!(!marker.exists());
    // Matching event with the matching tool fires.
    let _ = registry.dispatch(HookEvent::PreToolUse, json!({ "tool_name": "Bash" }));
    assert!(marker.exists(), "expected hook to create marker file");

    let _ = fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn skill_hook_once_self_removes_after_first_run() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_workspace("skill_hook_once");
    let counter = root.join("count");
    fs::write(&counter, "0").expect("init counter");
    let script = root.join("hook.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nn=$(cat {0})\necho $((n + 1)) > {0}\n",
            counter.display()
        ),
    )
    .expect("write hook script");
    let mut perms = fs::metadata(&script).expect("script meta").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).expect("chmod hook");

    let spec = SkillHookSpec {
        command: script.display().to_string(),
        once: true,
    };
    let handler = SkillHookHandler::new(
        "validator".to_string(),
        HookEvent::PreToolUse,
        None,
        spec,
        root.clone(),
    );
    let mut registry = HookRegistry::new();
    registry.register(Box::new(handler));

    let _ = registry.dispatch(HookEvent::PreToolUse, json!({ "tool_name": "Bash" }));
    let _ = registry.dispatch(HookEvent::PreToolUse, json!({ "tool_name": "Bash" }));
    let _ = registry.dispatch(HookEvent::PreToolUse, json!({ "tool_name": "Bash" }));
    let count = fs::read_to_string(&counter).expect("read counter");
    assert_eq!(count.trim(), "1", "once: true must fire exactly once");

    let _ = fs::remove_dir_all(root);
}
