use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use squeezy_core::SkillsConfig;

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
    };

    let catalog = SkillCatalog::discover(&root, &config);
    let first = catalog.load("rust-nav").expect("load first");

    fs::remove_file(skill_dir.join("SKILL.md")).expect("remove skill file");

    let second = catalog.load("rust-nav").expect("load second from cache");
    assert_eq!(first.body, second.body);

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

fn write_skill(dir: &Path, name: &str, description: &str, triggers: &[&str]) {
    fs::create_dir_all(dir).expect("mkdir");
    let triggers = triggers
        .iter()
        .map(|trigger| format!("  - {trigger}"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: {description}\ntriggers:\n{triggers}\n---\n# {name}\n"
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
